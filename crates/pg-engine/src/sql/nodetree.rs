//! `nodeToString` of a SQL-standard function body's analyzed statements, as
//! pg_proc.prosqlbody holds it: a `RETURN` body's Query, or a `BEGIN ATOMIC` body's list
//! of Queries in a list.
//!
//! The statements are analyzed again here the way Postgres's parse analysis builds its
//! Query trees: range tables (relations, joins, subqueries, functions, VALUES, CTEs and the
//! GROUP BY step's RTE_GROUP), permission entries, the join tree, target lists with their
//! sort/group references, window clauses, set operations, and INSERT / UPDATE / DELETE with
//! their RETURNING lists and ON CONFLICT. The binder binds the expressions over scopes this
//! builds; a column it hands back is mapped to the Var Postgres would make. Anything this
//! doesn't model gives `None` (and pg_proc shows the body's text).

use crate::catalog::system::{SysTable, collation_oid, type_collation};
use crate::catalog::{Function, Relation, Table};
use crate::exec::ExecCtx;
use crate::sql::binder::{
    Binder, Clause, Coll, CteDef, Level, Scope, ScopeCol, ScopeRel, ast, derived_rel,
    table_rel,
};
use crate::sql::expr::{BoolTest, Expr, SubqueryKind};
use crate::sql::func::pg_oids::{OPERS, Oper, PROCS};
use crate::sql::plan::{OutCol, WinFunc, WinKind};
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf::{self as pb, Node};
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write;
use std::sync::{Arc, OnceLock};

/// prosqlbody of `f`, when its statements are all ones this models.
pub fn sql_body_tree(ctx: &ExecCtx, f: &Function) -> Option<String> {
    let kind = f.sql_body?;
    let stmts = crate::sql::parse::parse(&f.body).ok()?;
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    b.level().barrier = true;
    let mut lvl = Level::new();
    lvl.barrier = true;
    lvl.fn_name = f.name.clone();
    lvl.params = f
        .args
        .iter()
        .enumerate()
        .map(|(i, (_, t))| Expr::Param(i, t.clone()))
        .collect();
    lvl.fn_args = Some(f.args.clone());
    b.levels.push(lvl);
    b.fn_levels.push(1);
    let mut t = Tree {
        b: &mut b,
        qs: HashMap::new(),
        subst: None,
        recursive_flag: false,
        rename: None,
        colls: Vec::new(),
    };
    let mut queries = Vec::new();
    for (i, stmt) in stmts.iter().enumerate() {
        let node = stmt.stmt.as_deref()?;
        let is_return = kind == 'r' || f.atomic_returns.contains(&i);
        let lvl = t.b.level();
        lvl.scope = Default::default();
        lvl.st = Default::default();
        t.qs.insert(1, QState::default());
        let q = t.statement(node, is_return)?;
        queries.push(q.text);
    }
    if kind == 'r' {
        let [q] = queries.as_slice() else {
            return None;
        };
        return Some(q.clone());
    }
    Some(format!("(({}))", queries.join(" ")))
}

/// A view's `_RETURN` rule action (pg_rewrite.ev_action): its query, with the view's column
/// names, as a list. `cat`: the catalog the view's query reads.
pub fn view_rule_tree(
    ctx: &ExecCtx,
    cat: &Arc<crate::catalog::Catalog>,
    query: &str,
    colnames: &[String],
) -> Option<String> {
    let stmts = crate::sql::parse::parse(query).ok()?;
    let [stmt] = stmts.as_slice() else {
        return None;
    };
    let NodeEnum::SelectStmt(s) = stmt.stmt.as_deref()?.node.as_ref()? else {
        return None;
    };
    let mut b = Binder::new(ctx, vec![]);
    b.cat = cat.clone();
    b.deparse = true;
    b.level().barrier = true;
    let mut lvl = Level::new();
    lvl.barrier = true;
    b.levels.push(lvl);
    let mut t = Tree {
        b: &mut b,
        qs: HashMap::new(),
        subst: None,
        recursive_flag: false,
        rename: Some(colnames.to_vec()),
        colls: Vec::new(),
    };
    t.qs.insert(1, QState::default());
    let q = t.select(s, false, true)?;
    Some(format!("({})", q.text))
}

/// pg_policy.polqual / polwithcheck: a policy's expression over its table (range-table
/// entry 1) as nodeToString prints it; None when it uses something this doesn't model.
pub fn policy_expr_tree(ctx: &ExecCtx, t: &Arc<Table>, sql: &str) -> Option<String> {
    let n = ast::parse_expr(sql).ok()?;
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    b.unplanned = true;
    b.level().barrier = true;
    let mut lvl = Level::new();
    lvl.barrier = true;
    b.levels.push(lvl);
    let mut tr = Tree {
        b: &mut b,
        qs: HashMap::new(),
        subst: None,
        recursive_flag: false,
        rename: None,
        colls: Vec::new(),
    };
    tr.qs.insert(1, QState::default());
    let names: Vec<String> = t
        .columns
        .iter()
        .map(|c| if c.dropped { String::new() } else { c.name.clone() })
        .collect();
    let (rte, perm) = tr.relation_rte(t.oid, 'r', None, &t.name, &names, true, 1, 0, false);
    let (rel, _) = tr.table_item(t, rte, perm, &t.name, None, &names)?;
    tr.b.level().scope = Scope { rels: vec![rel] };
    let e = tr
        .b
        .in_clause(Clause::Policy, |b| {
            let e = b.bind_expr(&n)?;
            b.coerce_bool(e, "POLICY")
        })
        .ok()?;
    let mut out = String::new();
    tr.expr(&mut out, &e, 1)?;
    Some(out)
}

// ── output helpers ──

/// pg_cast: (source, target) → (castfunc, castmethod).
fn casts() -> &'static HashMap<(u32, u32), (u32, char)> {
    static C: OnceLock<HashMap<(u32, u32), (u32, char)>> = OnceLock::new();
    C.get_or_init(|| {
        include_str!("../catalog/data/pg_cast.tsv")
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                Some((
                    (f.get(1)?.parse().ok()?, f.get(2)?.parse().ok()?),
                    (f.get(3)?.parse().ok()?, f.get(5)?.chars().next()?),
                ))
            })
            .collect()
    })
}

/// The operator `name` over (left, right) (0: none): exactly, else the one declared over
/// polymorphic pseudo-types that matches the most argument types exactly
/// (`text || anynonarray`).
fn oper(name: &str, left: u32, right: u32) -> Option<&'static Oper> {
    oper_typed(name, left, right, false, false)
}

fn oper_typed(
    name: &str,
    left: u32,
    right: u32,
    left_array: bool,
    right_array: bool,
) -> Option<&'static Oper> {
    let fits = |want: u32, have: u32, array: bool| -> Option<u32> {
        match want {
            _ if want == have => Some(1),
            _ if have == 0 => None,
            2277 | 5078 => array.then_some(0),
            2776 | 5079 => (!array).then_some(0),
            2276 | 2283 | 5077 => Some(0),
            // anyrange / anycompatiblerange, anymultirange / anycompatiblemultirange.
            3831 | 5080 => matches!(DataType::from_oid(have), Some(DataType::Range(_))).then_some(0),
            4537 | 4538 => {
                matches!(DataType::from_oid(have), Some(DataType::Multirange(_))).then_some(0)
            }
            _ => None,
        }
    };
    OPERS
        .iter()
        .filter(|o| o.name == name)
        .filter_map(|o| {
            Some((
                fits(o.left, left, left_array).or((o.left == 0 && left == 0).then_some(1))?
                    + fits(o.right, right, right_array)?,
                o,
            ))
        })
        .max_by_key(|(n, o)| (*n, std::cmp::Reverse(o.oid)))
        .map(|(_, o)| o)
}

/// The operator `name` over two values of `ty`: its oid, and whether it is declared over
/// polymorphic types.
pub(crate) fn operator_oid(name: &str, ty: &DataType) -> Option<(u32, bool)> {
    let res = crate::sql::func::resolve_binop(name, ty, ty).ok()?;
    let (l, r) = (res.arg_types.first()?, res.arg_types.get(1)?);
    let o = oper_typed(name, l.oid(), r.oid(), l.is_array(), r.is_array())?;
    Some((o.oid, o.left != l.oid() || o.right != r.oid()))
}

/// The type's default btree ordering: (eqop, ltop or gtop, hashable).
fn sort_ops(ty: &DataType, desc: bool) -> Option<(u32, u32, bool)> {
    let t = ty.oid();
    let (eq, ord) = if ty.is_array() {
        (oper_typed("=", t, t, true, true)?, oper_typed(if desc { ">" } else { "<" }, t, t, true, true)?)
    } else {
        (oper("=", t, t)?, oper(if desc { ">" } else { "<" }, t, t)?)
    };
    // An anonymous record's fields can't be checked for hashing (the type cache's
    // record_fields_have_hashing), nor can an array of them.
    let anon = matches!(ty, DataType::Record) || matches!(ty, DataType::Array(e) if **e == DataType::Record);
    Some((eq.oid, ord.oid, eq.hashes && !anon))
}

/// `outToken`.
fn token(out: &mut String, s: &str) {
    if s.is_empty() {
        out.push_str("<>");
        return;
    }
    let mut it = s.chars();
    let first = it.next().unwrap_or(' ');
    let second = it.next();
    if first == '<'
        || first == '"'
        || first.is_ascii_digit()
        || (matches!(first, '+' | '-')
            && second.is_some_and(|c| c.is_ascii_digit() || c == '.'))
    {
        out.push('\\');
    }
    for c in s.chars() {
        if matches!(c, ' ' | '\n' | '\t' | '(' | ')' | '{' | '}' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
}

/// A String node in a list.
fn string_node(out: &mut String, s: &str) {
    out.push('"');
    if !s.is_empty() {
        token(out, s);
    }
    out.push('"');
}

fn names_list(names: &[String]) -> String {
    if names.is_empty() {
        return "<>".into();
    }
    let mut out = String::from("(");
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        string_node(&mut out, n);
    }
    out.push(')');
    out
}

fn list(items: &[String]) -> String {
    if items.is_empty() {
        "<>".to_string()
    } else {
        format!("({})", items.join(" "))
    }
}

/// An integer / oid list (`(i 1 2)`, `(o 23)`).
fn int_list<T: std::fmt::Display>(tag: char, items: &[T]) -> String {
    if items.is_empty() {
        return "<>".into();
    }
    let mut out = format!("({tag}");
    for i in items {
        let _ = write!(out, " {i}");
    }
    out.push(')');
    out
}

fn bitmap(set: &BTreeSet<i32>) -> String {
    let mut out = String::from("(b");
    for i in set {
        let _ = write!(out, " {i}");
    }
    out.push(')');
    out
}

fn alias_node(name: &str, colnames: &[String]) -> String {
    let mut out = String::from("{ALIAS :aliasname ");
    token(&mut out, name);
    out.push_str(" :colnames ");
    out.push_str(&names_list(colnames));
    out.push('}');
    out
}

fn coll_oid(c: &Coll) -> u32 {
    match c {
        Coll::Implicit(n) | Coll::Explicit(n, _) => collation_oid(n).unwrap_or(0),
        _ => 0,
    }
}

/// The `[ bytes ]` of a datum (`outDatum`): a by-value one as the 8 bytes of its Datum.
fn datum(
    out: &mut String,
    v: &Value,
    ty: &DataType,
    enum_oid: &dyn Fn(u32, &str) -> u32,
) -> Option<()> {
    let m = crate::types::ondisk::meta(ty)?;
    let mut bytes = crate::types::ondisk::datum_bytes(v, ty, enum_oid)?;
    let len = bytes.len();
    if m.byval {
        // Signed narrower values are sign-extended into the Datum (oids are not).
        let signed = matches!(
            ty,
            DataType::Int2 | DataType::Int4 | DataType::Date | DataType::Float4
        );
        let neg = signed && bytes.last().is_some_and(|b| b & 0x80 != 0);
        bytes.resize(8, if neg { 0xff } else { 0 });
    }
    let _ = write!(out, "{len} [ ");
    for b in bytes {
        let _ = write!(out, "{b} ");
    }
    out.push(']');
    Some(())
}

/// A builtin function's oid by name and argument types.
/// pg_proc.proretset of a built-in function.
fn proc_retset(oid: u32) -> Option<bool> {
    crate::catalog::system::builtin_procs()
        .iter()
        .find(|r| matches!(r[0], Value::Oid(o) if o == oid))
        .and_then(|r| match r.get(13) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        })
}

fn proc_oid(name: &str, args: &[u32]) -> Option<u32> {
    PROCS
        .iter()
        .find(|p| p.1 == name && p.2 == args)
        .or_else(|| {
            // One declared over polymorphic pseudo-types.
            PROCS.iter().find(|p| {
                p.1 == name
                    && p.2.len() == args.len()
                    && p.2.iter().zip(args).all(|(w, h)| {
                        w == h || matches!(*w, 2276 | 2283 | 2277 | 2776 | 3500 | 5077 | 5078 | 5079)
                    })
            })
        })
        .map(|p| p.0)
}

// ── analysis state ──

/// What a column the binder hands back (`Col(i)`) stands for: a Var.
#[derive(Clone, Debug)]
struct VarRef {
    rte: usize,
    attno: i32,
    ty: DataType,
    vartype: u32,
    typmod: i32,
    coll: u32,
    /// The permission entry a reference marks as read.
    perm: Option<usize>,
    /// Read through a join's alias: that join and the column's position in it
    /// (varnosyn / varattnosyn).
    syn: Option<(usize, i32)>,
    /// The table column it comes from (resorigtbl / resorigcol of a TLE that is it).
    origin: (u32, i32),
}

#[derive(Clone, Debug, Default)]
struct Perm {
    relid: u32,
    inh: bool,
    required: u32,
    selected: BTreeSet<i32>,
    inserted: BTreeSet<i32>,
    updated: BTreeSet<i32>,
}

/// A CTE defined by a WITH of the query at some level.
#[derive(Clone, Debug)]
struct Cte {
    name: String,
    aliascols: Vec<String>,
    materialized: i32,
    query: String,
    cols: Vec<OutInfo>,
    refcount: usize,
    recursive: bool,
    /// Its own query is being analyzed: references are self-references.
    in_progress: bool,
    /// SEARCH / CYCLE: the columns they add (references see them) and their nodes.
    extra: Vec<OutInfo>,
    clauses: String,
}

/// An output column of a query (for the levels that read it).
#[derive(Clone, Debug)]
struct OutInfo {
    name: String,
    ty: DataType,
    typmod: i32,
    coll: u32,
    collation: Option<Coll>,
    origin: (u32, i32),
}

impl OutInfo {
    fn out_col(&self) -> OutCol {
        let mut c = OutCol::new(self.name.clone(), self.ty.clone());
        c.table = self.origin.0;
        c.attnum = self.origin.1 as i16;
        c.collation = self.collation.clone();
        c
    }
}

/// One query level being analyzed.
#[derive(Default)]
struct QState {
    rtable: Vec<String>,
    perms: Vec<Perm>,
    cols: HashMap<usize, VarRef>,
    next_col: usize,
    /// Per range-table entry: the outer joins (their RT indexes) that can null its columns.
    nulled: HashMap<usize, BTreeSet<usize>>,
    /// GROUP BY expressions (the RTE_GROUP's columns) and that entry's index.
    group: Vec<Expr>,
    /// Per GROUP BY expression: grouping sets can null it.
    group_nullable: Vec<bool>,
    /// GROUPING()'s view of the GROUP BY expressions (`Expr::Grouping` positions) and
    /// their sort/group refs.
    grouping_args: Vec<Expr>,
    grouping_refs: Vec<usize>,
    group_rte: usize,
    ctes: Vec<Cte>,
    has_sublinks: bool,
    /// The query has WITH RECURSIVE.
    has_recursive: bool,
    /// Window clause numbers of the level's window function calls.
    winrefs: Vec<usize>,
    /// The column position standing for merge_action() in a MERGE's RETURNING.
    merge_action: Option<usize>,
}

/// A FROM item's column as a reference resolves it: its name and the Var.
#[derive(Clone)]
struct NsCol {
    name: String,
    col: usize,
}

/// An analyzed query.
struct QueryOut {
    text: String,
    cols: Vec<OutInfo>,
}

/// GROUP BY being analyzed: the SortGroupClauses, the grouping expressions and their
/// sort/group refs.
struct GroupAcc {
    groups: Vec<String>,
    exprs: Vec<Expr>,
    refs: Vec<usize>,
}

/// A target list entry being built.
struct Tle {
    expr: Expr,
    name: Option<String>,
    sortref: usize,
    junk: bool,
    origin: (u32, i32),
    resno: Option<i32>,
}

struct Tree<'q, 'b, 'a> {
    b: &'q mut Binder<'b, 'a>,
    qs: HashMap<usize, QState>,
    /// Expressions at this level matching a GROUP BY expression read the RTE_GROUP.
    subst: Option<usize>,
    /// The query just written has WITH RECURSIVE (`cte_list` → `finish`).
    recursive_flag: bool,
    /// A view's column names, for its query's output (level 1).
    rename: Option<Vec<String>>,
    /// The collations of the nodes of the expressions being written (`expr`), innermost
    /// last: computed once per expression rather than once per node asked.
    colls: Vec<HashMap<usize, Option<Coll>>>,
}

impl Tree<'_, '_, '_> {
    fn lvl(&self) -> usize {
        self.b.levels.len() - 1
    }

    fn q(&mut self) -> &mut QState {
        let l = self.lvl();
        self.qs.get_mut(&l).unwrap()
    }

    /// Analyze a subquery at a new level.
    fn nested<T>(&mut self, f: impl FnOnce(&mut Self) -> Option<T>) -> Option<T> {
        self.b.levels.push(Level::new());
        let lvl = self.lvl();
        self.qs.insert(lvl, QState::default());
        let saved = self.subst.take();
        let r = f(self);
        self.subst = saved;
        self.qs.remove(&lvl);
        self.b.levels.pop();
        r
    }

    fn statement(&mut self, n: &Node, is_return: bool) -> Option<QueryOut> {
        match n.node.as_ref()? {
            NodeEnum::SelectStmt(s) => self.select(s, is_return, true),
            NodeEnum::InsertStmt(s) => self.insert(s),
            NodeEnum::UpdateStmt(s) => self.update(s),
            NodeEnum::DeleteStmt(s) => self.delete(s),
            NodeEnum::MergeStmt(s) => self.merge(s),
            _ => None,
        }
    }

    // ── range table ──

    fn add_rte(&mut self, text: String) -> usize {
        let q = self.q();
        q.rtable.push(text);
        q.rtable.len()
    }

    /// A column position for the binder standing for `v`.
    fn new_col(&mut self, v: VarRef) -> usize {
        let q = self.q();
        let c = q.next_col;
        q.next_col += 1;
        q.cols.insert(c, v);
        c
    }

    /// A relation in the range table, with its permission entry.
    #[allow(clippy::too_many_arguments)]
    fn relation_rte(
        &mut self,
        relid: u32,
        relkind: char,
        alias: Option<&pb::Alias>,
        refname: &str,
        colnames: &[String],
        inh: bool,
        lockmode: i32,
        required: u32,
        in_from: bool,
    ) -> (usize, usize) {
        let q = self.q();
        q.perms.push(Perm {
            relid,
            inh,
            required,
            ..Default::default()
        });
        let perm = q.perms.len();
        let alias_text = match alias {
            Some(a) => alias_node(&a.aliasname, &ast::names(&a.colnames)),
            None => "<>".into(),
        };
        let mut eref = colnames.to_vec();
        if let Some(a) = alias {
            for (c, n) in eref.iter_mut().filter(|c| !c.is_empty()).zip(ast::names(&a.colnames)) {
                *c = n;
            }
        }
        let text = format!(
            "{{RANGETBLENTRY :alias {alias_text} :eref {} :rtekind 0 :relid {relid} :inh {inh} :relkind {relkind} :rellockmode {lockmode} :perminfoindex {perm} :tablesample <> :lateral false :inFromCl {in_from} :securityQuals <>}}",
            alias_node(refname, &eref)
        );
        (self.add_rte(text), perm - 1)
    }

    /// A table's columns as a FROM item.
    fn table_item(
        &mut self,
        t: &Arc<Table>,
        rte: usize,
        perm: usize,
        refname: &str,
        schema: Option<String>,
        colnames: &[String],
    ) -> Option<(ScopeRel, Vec<NsCol>)> {
        let base = self.q().next_col;
        let mut rel = Scope {
            rels: vec![table_rel(t, refname, schema)],
        }
        .shifted(base)
        .rels
        .pop()?;
        let mut ns = Vec::new();
        let mut names = colnames.iter();
        for (i, c) in t.columns.iter().enumerate() {
            let col = self.new_col(VarRef {
                rte,
                attno: i as i32 + 1,
                ty: c.ty.clone(),
                vartype: c.domain.unwrap_or_else(|| c.ty.oid()),
                typmod: c.ty.typmod(),
                coll: crate::catalog::system::column_collation(c),
                perm: Some(perm),
                syn: None,
                origin: (t.oid, i as i32 + 1),
            });
            debug_assert_eq!(col, base + i);
            if !c.dropped {
                let name = names.next().cloned().unwrap_or_else(|| c.name.clone());
                rel.cols[i].name = name.clone();
                ns.push(NsCol { name, col });
            }
        }
        Some((rel, ns))
    }

    /// Columns of a derived FROM item (subquery, view, function, CTE) as a scope.
    fn derived_item(
        &mut self,
        refname: &str,
        rte: usize,
        perm: Option<usize>,
        cols: &[OutInfo],
    ) -> (ScopeRel, Vec<NsCol>) {
        let base = self.q().next_col;
        let outs: Vec<OutCol> = cols.iter().map(OutInfo::out_col).collect();
        let rel = derived_rel(refname, &outs, base);
        let mut ns = Vec::new();
        for (i, c) in cols.iter().enumerate() {
            let col = self.new_col(VarRef {
                rte,
                attno: i as i32 + 1,
                ty: c.ty.clone(),
                vartype: c.ty.oid(),
                typmod: c.typmod,
                coll: c.coll,
                perm,
                syn: None,
                origin: c.origin,
            });
            ns.push(NsCol {
                name: c.name.clone(),
                col,
            });
        }
        (rel, ns)
    }

    /// One FROM item: its join-tree node, scope and columns. Its range-table entries are
    /// added.
    fn range_item(&mut self, n: &Node) -> Option<(String, Scope, Vec<NsCol>, Vec<usize>)> {
        match n.node.as_ref()? {
            NodeEnum::RangeVar(rv) => {
                let (rte, rel, ns) = self.range_var(rv)?;
                Some((
                    format!("{{RANGETBLREF :rtindex {rte}}}"),
                    Scope { rels: vec![rel] },
                    ns,
                    vec![rte],
                ))
            }
            NodeEnum::RangeSubselect(rs) => {
                let sub = rs.subquery.as_deref()?;
                let NodeEnum::SelectStmt(s) = sub.node.as_ref()? else {
                    return None;
                };
                let out = self.nested(|t| t.select(s, false, true))?;
                let alias = rs.alias.as_ref()?;
                let mut cols = out.cols.clone();
                for (c, n) in cols.iter_mut().zip(ast::names(&alias.colnames)) {
                    c.name = n;
                }
                let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
                let text = format!(
                    "{{RANGETBLENTRY :alias {} :eref {} :rtekind 1 :subquery {} :security_barrier false :relid 0 :inh false :relkind <> :rellockmode 0 :perminfoindex 0 :lateral {} :inFromCl true :securityQuals <>}}",
                    alias_node(&alias.aliasname, &ast::names(&alias.colnames)),
                    alias_node(&alias.aliasname, &names),
                    out.text,
                    rs.lateral
                );
                let rte = self.add_rte(text);
                let (rel, ns) = self.derived_item(&alias.aliasname, rte, None, &cols);
                Some((
                    format!("{{RANGETBLREF :rtindex {rte}}}"),
                    Scope { rels: vec![rel] },
                    ns,
                    vec![rte],
                ))
            }
            NodeEnum::RangeFunction(rf) => self.range_function(rf),
            NodeEnum::JoinExpr(j) => self.join(j),
            _ => None,
        }
    }

    fn range_var(&mut self, rv: &pb::RangeVar) -> Option<(usize, ScopeRel, Vec<NsCol>)> {
        // (An alias the engine added to a renamed relation names it, but isn't one.)
        let alias = ast::written_alias(rv);
        let colnames = (rv.alias.as_ref())
            .map(|a| ast::names(&a.colnames))
            .unwrap_or_default();
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        // A CTE of this query or an enclosing one.
        if schema.is_none() {
            let lvl = self.lvl();
            for l in (1..=lvl).rev() {
                let Some(q) = self.qs.get_mut(&l) else {
                    continue;
                };
                let Some(c) = q.ctes.iter_mut().find(|c| c.name == rv.relname) else {
                    continue;
                };
                let self_ref = c.in_progress;
                if !self_ref {
                    c.refcount += 1;
                }
                let mut cols = c.cols.clone();
                cols.extend(c.extra.iter().cloned());
                for (c, n) in cols.iter_mut().zip(&colnames) {
                    c.name = n.clone();
                }
                let refname = alias.map_or(rv.relname.as_str(), |a| a.aliasname.as_str());
                let text = format!(
                    "{{RANGETBLENTRY :alias {} :eref {} :rtekind 6 :ctename {} :ctelevelsup {} :self_reference {self_ref} :coltypes {} :coltypmods {} :colcollations {} :lateral false :inFromCl true :securityQuals <>}}",
                    alias.map_or("<>".into(), |a| alias_node(&a.aliasname, &colnames)),
                    alias_node(refname, &cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
                    {
                        let mut s = String::new();
                        token(&mut s, &rv.relname);
                        s
                    },
                    lvl - l,
                    int_list('o', &cols.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
                    int_list('i', &cols.iter().map(|c| c.typmod).collect::<Vec<_>>()),
                    int_list('o', &cols.iter().map(|c| c.coll).collect::<Vec<_>>()),
                );
                let rte = self.add_rte(text);
                let refname = refname.to_string();
                let (rel, ns) = self.derived_item(&refname, rte, None, &cols);
                return Some((rte, rel, ns));
            }
        }
        let rel = self.b.cat.find(schema, &rv.relname, &self.b.ctx.search_path);
        let refname = ast::rv_refname(rv).to_string();
        let schema_of = |b: &Binder, s: u32| {
            alias
                .is_none()
                .then(|| b.cat.schema_name(s).to_string())
        };
        match rel {
            Some(Relation::Table(t)) => {
                let names: Vec<String> = t
                    .columns
                    .iter()
                    .map(|c| if c.dropped { String::new() } else { c.name.clone() })
                    .collect();
                let relkind = if t.partkey.is_some() { 'p' } else { 'r' };
                let (rte, perm) =
                    self.relation_rte(t.oid, relkind, alias, &refname, &names, rv.inh, 1, 2, true);
                let schema = schema_of(self.b, t.schema);
                let (rel, ns) = self.table_item(&t, rte, perm, &refname, schema, &colnames)?;
                Some((rte, rel, ns))
            }
            Some(Relation::View(v)) if v.storage.is_none() => {
                let names: Vec<String> = v.columns.iter().map(|c| c.0.clone()).collect();
                let (rte, perm) =
                    self.relation_rte(v.oid, 'v', alias, &refname, &names, rv.inh, 1, 2, true);
                let cols: Vec<OutInfo> = v
                    .columns
                    .iter()
                    .enumerate()
                    .map(|(i, (n, ty))| {
                        let coll = v.collations.get(i).cloned().flatten();
                        OutInfo {
                            name: colnames.get(i).cloned().unwrap_or_else(|| n.clone()),
                            ty: ty.clone(),
                            typmod: ty.typmod(),
                            coll: coll
                                .as_deref()
                                .and_then(collation_oid)
                                .unwrap_or_else(|| type_collation(ty)),
                            collation: coll.map(Coll::Implicit),
                            origin: (v.oid, i as i32 + 1),
                        }
                    })
                    .collect();
                let (mut rel, ns) = self.derived_item(&refname, rte, Some(perm), &cols);
                rel.schema = schema_of(self.b, v.schema);
                Some((rte, rel, ns))
            }
            Some(_) => None,
            None => {
                let st = SysTable::lookup(schema, &rv.relname, &self.b.ctx.search_path)?;
                let relkind = sys_relkind(st.oid())?;
                let cols: Vec<OutInfo> = st
                    .columns()
                    .iter()
                    .enumerate()
                    .map(|(i, (n, ty))| {
                        let coll = crate::catalog::system::sys_column_collation(st.oid(), i + 1);
                        OutInfo {
                            name: colnames.get(i).cloned().unwrap_or_else(|| n.clone()),
                            ty: ty.clone(),
                            typmod: ty.typmod(),
                            coll: coll.and_then(collation_oid).unwrap_or_else(|| type_collation(ty)),
                            collation: coll.map(|c| Coll::Implicit(c.to_string())),
                            origin: (st.oid(), i as i32 + 1),
                        }
                    })
                    .collect();
                let names: Vec<String> = st.columns().iter().map(|c| c.0.clone()).collect();
                let (rte, perm) = self.relation_rte(
                    st.oid(),
                    relkind,
                    alias,
                    &refname,
                    &names,
                    rv.inh,
                    1,
                    2,
                    true,
                );
                let (mut rel, ns) = self.derived_item(&refname, rte, Some(perm), &cols);
                rel.sys_table = Some(st.oid());
                if alias.is_none() {
                    rel.schema = Some(if st.schema() == crate::catalog::PG_CATALOG_OID {
                        "pg_catalog".into()
                    } else {
                        "information_schema".into()
                    });
                }
                Some((rte, rel, ns))
            }
        }
    }

    fn range_function(
        &mut self,
        rf: &pb::RangeFunction,
    ) -> Option<(String, Scope, Vec<NsCol>, Vec<usize>)> {
        if rf.functions.is_empty() || !rf.is_rowsfrom && rf.functions.len() != 1 {
            return None;
        }
        let alias = rf.alias.as_ref();
        if rf.functions.len() > 1 && alias.is_none() {
            return None;
        }
        let lvl = self.lvl();
        let mut items = Vec::new();
        let mut infos: Vec<OutInfo> = Vec::new();
        let mut lateral = rf.lateral;
        let mut first_name = String::new();
        let mut scalar_single = false;
        for f in &rf.functions {
            let NodeEnum::List(l) = f.node.as_ref()? else {
                return None;
            };
            let fnode = l.items.first()?;
            // The column definitions: ROWS FROM's per function, else the whole item's.
            let defs: Vec<&Node> = match l.items.get(1).and_then(ast::inner) {
                Some(NodeEnum::List(d)) => d.items.iter().collect(),
                Some(_) => return None,
                None if rf.functions.len() == 1 => rf.coldeflist.iter().collect(),
                None => Vec::new(),
            };
            let (call, mut cols) = self
                .b
                .in_clause(Clause::FromFunction, |b| b.bind_from_function(fnode, &defs, true))
                .ok()?;
            if call.body.is_some() || call.func.oid == 0 || defs.is_empty() && cols.len() != 1 {
                return None;
            }
            if !PROCS.iter().any(|p| p.0 == call.func.oid) && call.func.oid < 16384 {
                return None;
            }
            // The function call, as the target list would have it (a record result when
            // the column definitions give its columns).
            let restype = if defs.is_empty() { call.ty.oid() } else { 2249 };
            let mut fx = String::new();
            let refs: Vec<&Expr> = call.args.iter().collect();
            let (input, result) = self.collations(&refs, &call.ty);
            let _ = write!(
                fx,
                "{{FUNCEXPR :funcid {} :funcresulttype {restype} :funcretset {} :funcvariadic false :funcformat 0 :funccollid {} :inputcollid {input} :args ",
                call.func.oid,
                proc_retset(call.func.oid).unwrap_or(call.func.is_srf()),
                if defs.is_empty() { result } else { 0 }
            );
            self.list_into(&mut fx, &refs, lvl)?;
            fx.push_str(" :location -1}");
            lateral |= call.args.iter().any(|a| !a.columns().is_empty());
            let fname = call.func.name.to_string();
            if first_name.is_empty() {
                first_name = fname.clone();
            }
            let coll_of = |t: &DataType| if t.collatable() { type_collation(t) } else { 0 };
            let col_text = if defs.is_empty() {
                // A scalar result's column is named after the function.
                cols[0].name = fname.clone();
                scalar_single = rf.functions.len() == 1;
                "1 :funccolnames <> :funccoltypes <> :funccoltypmods <> :funccolcollations <>".to_string()
            } else {
                format!(
                    "{} :funccolnames {} :funccoltypes {} :funccoltypmods {} :funccolcollations {}",
                    cols.len(),
                    names_list(&cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
                    int_list('o', &cols.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
                    int_list('i', &cols.iter().map(|c| c.ty.typmod()).collect::<Vec<_>>()),
                    int_list('o', &cols.iter().map(|c| coll_of(&c.ty)).collect::<Vec<_>>()),
                )
            };
            items.push(format!(
                "{{RANGETBLFUNCTION :funcexpr {fx} :funccolcount {col_text} :funcparams (b)}}"
            ));
            for c in &cols {
                infos.push(OutInfo {
                    name: c.name.clone(),
                    ty: c.ty.clone(),
                    typmod: c.ty.typmod(),
                    coll: if !defs.is_empty() {
                        coll_of(&c.ty)
                    } else if c.ty.collatable() {
                        result
                    } else {
                        0
                    },
                    collation: None,
                    origin: (0, 0),
                });
            }
        }
        let refname = alias.map_or(first_name.clone(), |a| a.aliasname.clone());
        let colnames = alias.map(|a| ast::names(&a.colnames)).unwrap_or_default();
        if rf.ordinality {
            infos.push(OutInfo {
                name: "ordinality".into(),
                ty: DataType::Int8,
                typmod: -1,
                coll: 0,
                collation: None,
                origin: (0, 0),
            });
        }
        // The alias names the columns in order; a lone scalar function's one takes the
        // alias's name.
        if scalar_single && colnames.is_empty() && let Some(a) = alias {
            infos[0].name = a.aliasname.clone();
        }
        for (c, n) in infos.iter_mut().zip(&colnames) {
            c.name = n.clone();
        }
        let text = format!(
            "{{RANGETBLENTRY :alias {} :eref {} :rtekind 3 :functions {} :funcordinality {} :lateral {lateral} :inFromCl true :securityQuals <>}}",
            alias.map_or("<>".into(), |a| alias_node(&a.aliasname, &colnames)),
            alias_node(&refname, &infos.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
            list(&items),
            rf.ordinality,
        );
        let rte = self.add_rte(text);
        let (rel, ns) = self.derived_item(&refname, rte, None, &infos);
        Some((
            format!("{{RANGETBLREF :rtindex {rte}}}"),
            Scope { rels: vec![rel] },
            ns,
            vec![rte],
        ))
    }

    fn join(&mut self, j: &pb::JoinExpr) -> Option<(String, Scope, Vec<NsCol>, Vec<usize>)> {
        use pb::JoinType as J;
        let jt = J::try_from(j.jointype).ok()?;
        let jcode = match jt {
            J::JoinInner => 0,
            J::JoinLeft => 1,
            J::JoinFull => 2,
            J::JoinRight => 3,
            _ => return None,
        };
        // The right side sees the left (LATERAL).
        let saved = std::mem::take(&mut self.b.level().scope);
        let (lnode, lscope, lcols, lrtes) = self.range_item(j.larg.as_deref()?)?;
        self.b.level().scope = lscope.clone();
        let r = self.range_item(j.rarg.as_deref()?);
        self.b.level().scope = saved;
        let (rnode, rscope, rcols, rrtes) = r?;
        let using: Vec<String> = if j.is_natural {
            let rnames: Vec<&str> = rcols.iter().map(|c| c.name.as_str()).collect();
            lcols
                .iter()
                .map(|c| c.name.clone())
                .filter(|n| rnames.contains(&n.as_str()))
                .collect()
        } else {
            ast::names(&j.using_clause)
        };
        let rtindex = self.q().rtable.len() + 1;
        let mut quals: Vec<Expr> = Vec::new();
        let mut merged_cols: Vec<NsCol> = Vec::new();
        let mut merged_rel = ScopeRel {
            alias: String::new(),
            anonymous: true,
            ..Default::default()
        };
        let mut alias_vars: Vec<Expr> = Vec::new();
        let (mut lpos, mut rpos) = (Vec::new(), Vec::new());
        let lvl = self.lvl();
        for (k, n) in using.iter().enumerate() {
            let li = lcols.iter().position(|c| &c.name == n)?;
            let ri = rcols.iter().position(|c| &c.name == n)?;
            if lcols.iter().filter(|c| &c.name == n).count() != 1
                || rcols.iter().filter(|c| &c.name == n).count() != 1
            {
                return None;
            }
            lpos.push(li + 1);
            rpos.push(ri + 1);
            let lv = self.col_expr(lcols[li].col)?;
            let rv = self.col_expr(rcols[ri].col)?;
            let eq = self.b.binop("=", lv.clone(), rv.clone(), -1).ok()?;
            quals.push(eq);
            let ty = crate::sql::binder::unify_types(&[lv.ty(), rv.ty()], "JOIN/USING").ok()?;
            // The merged column: a side's column, or (FULL, or coerced) the join's own.
            let (col, value) = match jcode {
                0 | 1 if lv.ty() == ty => (lcols[li].col, lv),
                0 | 3 if rv.ty() == ty => (rcols[ri].col, rv),
                _ => {
                    let l = self.b.coerce_implicit(lv.clone(), &ty).ok()?;
                    let r = self.b.coerce_implicit(rv.clone(), &ty).ok()?;
                    let value = match jcode {
                        2 => Expr::Coalesce(vec![l, r], ty.clone()),
                        3 => r,
                        _ => l,
                    };
                    let coll = self.collations(&[&value], &ty).1;
                    let c = self.new_col(VarRef {
                        rte: rtindex,
                        attno: k as i32 + 1,
                        ty: ty.clone(),
                        vartype: ty.oid(),
                        typmod: ty.typmod(),
                        coll,
                        perm: None,
                        syn: None,
                        origin: (0, 0),
                    });
                    (c, value)
                }
            };
            alias_vars.push(value);
            let e = self.col_expr(col)?;
            merged_rel.cols.push(ScopeCol {
                name: n.clone(),
                expr: e,
                hidden: false,
                merged: false,
                table: 0,
                attnum: 0,
                collation: None,
                src: None,
                rowtype_col: None,
                domain: None,
            });
            merged_cols.push(NsCol {
                name: n.clone(),
                col,
            });
        }
        let _ = lvl;
        let (mut lscope, mut rscope) = (lscope, rscope);
        for n in &using {
            for s in [&mut lscope, &mut rscope] {
                for r in &mut s.rels {
                    for c in &mut r.cols {
                        if &c.name == n && !c.hidden {
                            c.merged = true;
                        }
                    }
                }
            }
        }
        let mut scope = Scope::default();
        if !using.is_empty() {
            scope.rels.push(merged_rel);
        }
        scope.extend(lscope);
        scope.extend(rscope);
        // ON, over both sides (not yet nulled by this join).
        if let Some(on) = j.quals.as_deref() {
            let saved = std::mem::replace(&mut self.b.level().scope, scope.clone());
            let r = self
                .b
                .in_clause(Clause::JoinOn, |b| {
                    let e = b.bind_expr(on)?;
                    b.coerce_bool_at(e, "JOIN/ON", on)
                })
                .ok();
            self.b.level().scope = saved;
            quals.push(r?);
        }
        let quals_text = match quals.len() {
            0 => "<>".to_string(),
            1 => self.expr_text(&quals[0])?,
            _ => self.expr_text(&Expr::And(quals))?,
        };
        // This join nulls the columns of its outer sides from here on.
        let mut nullable: Vec<usize> = Vec::new();
        if matches!(jcode, 1 | 2) {
            nullable.extend(&rrtes);
        }
        if matches!(jcode, 2 | 3) {
            nullable.extend(&lrtes);
        }
        for r in nullable {
            self.q().nulled.entry(r).or_default().insert(rtindex);
        }
        // The join's columns: merged ones, then the left's and the right's others.
        let rest = |cols: &[NsCol]| -> Vec<(usize, NsCol)> {
            cols.iter()
                .enumerate()
                .filter(|(_, c)| !using.contains(&c.name))
                .map(|(i, c)| (i + 1, c.clone()))
                .collect()
        };
        let lrest = rest(&lcols);
        let rrest = rest(&rcols);
        let mut out_cols: Vec<NsCol> = merged_cols.clone();
        let mut vars = Vec::new();
        for v in &alias_vars {
            vars.push(self.expr_text_nomark(v)?);
        }
        for (i, c) in lrest.iter().chain(&rrest) {
            let _ = i;
            let e = self.col_expr(c.col)?;
            vars.push(self.expr_text_nomark(&e)?);
            out_cols.push(c.clone());
        }
        lpos.extend(lrest.iter().map(|(i, _)| *i));
        rpos.extend(rrest.iter().map(|(i, _)| *i));
        let mut colnames: Vec<String> = out_cols.iter().map(|c| c.name.clone()).collect();
        let (alias_text, refname) = match &j.alias {
            Some(a) => {
                for (c, n) in colnames.iter_mut().zip(ast::names(&a.colnames)) {
                    *c = n;
                }
                (
                    alias_node(&a.aliasname, &ast::names(&a.colnames)),
                    a.aliasname.clone(),
                )
            }
            None => ("<>".into(), "unnamed_join".into()),
        };
        let using_alias = match &j.join_using_alias {
            Some(a) => alias_node(&a.aliasname, &ast::names(&a.colnames)),
            None => "<>".into(),
        };
        let text = format!(
            "{{RANGETBLENTRY :alias {alias_text} :eref {} :rtekind 2 :jointype {jcode} :joinmergedcols {} :joinaliasvars {} :joinleftcols {} :joinrightcols {} :join_using_alias {using_alias} :lateral false :inFromCl true :securityQuals <>}}",
            alias_node(&refname, &colnames),
            using.len(),
            list(&vars),
            int_list('i', &lpos),
            int_list('i', &rpos),
        );
        let got = self.add_rte(text);
        if got != rtindex {
            return None;
        }
        // A join alias hides the names inside: its columns read as the join's.
        if let Some(a) = &j.alias {
            let mut rel = ScopeRel {
                alias: a.aliasname.clone(),
                ..Default::default()
            };
            for (k, c) in out_cols.iter_mut().enumerate() {
                let mut v = self.q().cols.get(&c.col)?.clone();
                v.syn = Some((rtindex, k as i32 + 1));
                c.col = self.new_col(v);
            }
            for (c, n) in out_cols.iter().zip(&colnames) {
                rel.cols.push(ScopeCol {
                    name: n.clone(),
                    expr: self.col_expr(c.col)?,
                    hidden: false,
                    merged: false,
                    table: 0,
                    attnum: 0,
                    collation: None,
                    src: None,
                    rowtype_col: None,
                    domain: None,
                });
            }
            scope = Scope { rels: vec![rel] };
            for (c, n) in out_cols.iter_mut().zip(&colnames) {
                c.name = n.clone();
            }
        }
        let using_text = names_list(&using);
        let node = format!(
            "{{JOINEXPR :jointype {jcode} :isNatural {} :larg {lnode} :rarg {rnode} :usingClause {using_text} :join_using_alias {using_alias} :quals {quals_text} :alias {alias_text} :rtindex {rtindex}}}",
            j.is_natural
        );
        let mut rtes = lrtes;
        rtes.extend(rrtes);
        rtes.push(rtindex);
        Some((node, scope, out_cols, rtes))
    }

    /// The binder's expression for column position `col` of the current level.
    fn col_expr(&mut self, col: usize) -> Option<Expr> {
        let v = self.q().cols.get(&col)?.clone();
        Some(Expr::Col(col, v.ty))
    }

    /// FROM: the range table, the join tree's list, and the scope.
    fn range_table_items(&mut self, items: &[Node]) -> Option<Vec<String>> {
        let mut nodes = Vec::new();
        for item in items {
            let (node, scope, _, _) = self.range_item(item)?;
            self.b.level().scope.extend(scope);
            nodes.push(node);
        }
        Some(nodes)
    }

    // ── WITH ──

    fn with_clause(&mut self, w: &pb::WithClause) -> Option<()> {
        self.q().has_recursive = w.recursive;
        self.b.ctes.push(Vec::new());
        for c in &w.ctes {
            let NodeEnum::CommonTableExpr(cte) = c.node.as_ref()? else {
                return None;
            };
            let q = cte.ctequery.as_deref()?;
            let NodeEnum::SelectStmt(s) = q.node.as_ref()? else {
                return None;
            };
            let aliases = ast::names(&cte.aliascolnames);
            // A recursive one: its columns are the non-recursive term's, which the
            // recursive term reads as itself.
            let self_ref = w.recursive
                && q.node.as_ref()?.nodes().into_iter().any(|(n, ..)| {
                    matches!(n, pg_query::NodeRef::RangeVar(rv) if rv.schemaname.is_empty() && rv.relname == cte.ctename)
                });
            if self_ref {
                if s.op != pb::SetOperation::SetopUnion as i32 || s.with_clause.is_some() {
                    return None;
                }
                let larg = s.larg.as_deref()?;
                let first = self.nested(|t| t.select(larg, false, true))?;
                let mut cols = first.cols.clone();
                for (c, n) in cols.iter_mut().zip(&aliases) {
                    c.name = n.clone();
                }
                let (extra, clauses) = self.search_cycle(cte)?;
                let id = self.b.fresh_id();
                self.b.ctes.last_mut()?.push(CteDef {
                    name: cte.ctename.clone(),
                    id,
                    cols: cols.iter().chain(&extra).map(OutInfo::out_col).collect(),
                    working: false,
                    aux: 0,
                    used: std::cell::Cell::new(false),
                    no_returning: false,
                    work_filter: None,
                    deferred: None,
                    inline: None,
                });
                self.q().ctes.push(Cte {
                    name: cte.ctename.clone(),
                    aliascols: aliases.clone(),
                    materialized: cte.ctematerialized,
                    query: String::new(),
                    cols,
                    refcount: 0,
                    recursive: true,
                    in_progress: true,
                    extra,
                    clauses,
                });
                let out = self.nested(|t| t.select(s, false, false))?;
                let c = self.q().ctes.last_mut()?;
                c.query = out.text;
                c.in_progress = false;
                for (c, o) in c.cols.iter_mut().zip(&out.cols) {
                    c.origin = o.origin;
                }
                continue;
            }
            if cte.search_clause.is_some() || cte.cycle_clause.is_some() {
                return None;
            }
            let out = self.nested(|t| t.select(s, false, false))?;
            let mut cols = out.cols.clone();
            for (c, n) in cols.iter_mut().zip(&aliases) {
                c.name = n.clone();
            }
            let id = self.b.fresh_id();
            self.b.ctes.last_mut()?.push(CteDef {
                name: cte.ctename.clone(),
                id,
                cols: cols.iter().map(OutInfo::out_col).collect(),
                working: false,
                aux: 0,
                used: std::cell::Cell::new(false),
                no_returning: false,
                work_filter: None,
                deferred: None,
                inline: None,
            });
            self.q().ctes.push(Cte {
                name: cte.ctename.clone(),
                aliascols: aliases,
                materialized: cte.ctematerialized,
                query: out.text,
                cols,
                refcount: 0,
                recursive: false,
                in_progress: false,
                extra: Vec::new(),
                clauses: ":search_clause <> :cycle_clause <>".into(),
            });
        }
        Some(())
    }

    /// A recursive CTE's SEARCH / CYCLE clause: the columns it adds (a sequence column; a
    /// mark and a path column) and its `:search_clause` / `:cycle_clause` nodes.
    fn search_cycle(&mut self, cte: &pb::CommonTableExpr) -> Option<(Vec<OutInfo>, String)> {
        let col = |name: &str, ty: DataType| OutInfo {
            name: name.to_string(),
            typmod: -1,
            coll: 0,
            ty,
            collation: None,
            origin: (0, 0),
        };
        let records = DataType::array_of(DataType::Record);
        let names = |l: &[Node]| -> Option<Vec<String>> {
            l.iter().map(|n| ast::sval(n).map(str::to_string)).collect()
        };
        let mut extra = Vec::new();
        let search = match &cte.search_clause {
            Some(sc) => {
                let mut seq = String::new();
                token(&mut seq, &sc.search_seq_column);
                extra.push(col(
                    &sc.search_seq_column,
                    if sc.search_breadth_first {
                        DataType::Record
                    } else {
                        records.clone()
                    },
                ));
                format!(
                    "{{CTESEARCHCLAUSE :search_col_list {} :search_breadth_first {} :search_seq_column {seq} :location -1}}",
                    names_list(&names(&sc.search_col_list)?),
                    sc.search_breadth_first
                )
            }
            None => "<>".into(),
        };
        let cycle = match &cte.cycle_clause {
            Some(cc) => {
                // The mark's values: TRUE / FALSE, else the two given, of a common type.
                let (value, default, ty) = match (cc.cycle_mark_value.as_deref(), cc.cycle_mark_default.as_deref()) {
                    (None, None) => (
                        Expr::Const(Value::Bool(true), DataType::Bool),
                        Expr::Const(Value::Bool(false), DataType::Bool),
                        DataType::Bool,
                    ),
                    (Some(v), Some(d)) => {
                        let v = self.b.in_clause(Clause::Select, |b| b.bind_expr(v)).ok()?;
                        let d = self.b.in_clause(Clause::Select, |b| b.bind_expr(d)).ok()?;
                        let ty = match (v.ty(), d.ty()) {
                            (DataType::Unknown, DataType::Unknown) => DataType::Text,
                            (DataType::Unknown, t) | (t, DataType::Unknown) => t,
                            (a, b) if a == b => a,
                            _ => return None,
                        };
                        let v = self.b.coerce_implicit(v, &ty).ok()?;
                        let d = self.b.coerce_implicit(d, &ty).ok()?;
                        (v, d, ty)
                    }
                    _ => return None,
                };
                let (neop, _) = operator_oid("<>", &ty)?;
                let (mut mark, mut path) = (String::new(), String::new());
                token(&mut mark, &cc.cycle_mark_column);
                token(&mut path, &cc.cycle_path_column);
                let mut coll = 0;
                if ty.collatable() {
                    coll = type_collation(&ty);
                }
                extra.push(OutInfo {
                    coll,
                    ..col(&cc.cycle_mark_column, ty.clone())
                });
                extra.push(col(&cc.cycle_path_column, records));
                format!(
                    "{{CTECYCLECLAUSE :cycle_col_list {} :cycle_mark_column {mark} :cycle_mark_value {} :cycle_mark_default {} :cycle_path_column {path} :location -1 :cycle_mark_type {} :cycle_mark_typmod -1 :cycle_mark_collation {coll} :cycle_mark_neop {neop}}}",
                    names_list(&names(&cc.cycle_col_list)?),
                    self.expr_text(&value)?,
                    self.expr_text(&default)?,
                    ty.oid(),
                )
            }
            None => "<>".into(),
        };
        Some((extra, format!(":search_clause {search} :cycle_clause {cycle}")))
    }

    fn cte_list(&mut self) -> String {
        let ctes = std::mem::take(&mut self.q().ctes);
        self.recursive_flag = std::mem::take(&mut self.q().has_recursive);
        let items: Vec<String> = ctes
            .iter()
            .map(|c| {
                let mut name = String::new();
                token(&mut name, &c.name);
                format!(
                    "{{COMMONTABLEEXPR :ctename {name} :aliascolnames {} :ctematerialized {} :ctequery {} {} :location -1 :cterecursive {} :cterefcount {} :ctecolnames {} :ctecoltypes {} :ctecoltypmods {} :ctecolcollations {}}}",
                    names_list(&c.aliascols),
                    c.materialized.max(1) - 1,
                    c.query,
                    c.clauses,
                    c.recursive,
                    c.refcount,
                    names_list(&c.cols.iter().map(|c| c.name.clone()).collect::<Vec<_>>()),
                    int_list('o', &c.cols.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
                    int_list('i', &c.cols.iter().map(|c| c.typmod).collect::<Vec<_>>()),
                    int_list('o', &c.cols.iter().map(|c| c.coll).collect::<Vec<_>>()),
                )
            })
            .collect();
        list(&items)
    }

    // ── SELECT ──

    /// A SELECT (a set operation, VALUES, or a plain one) at the current level.
    fn select(&mut self, s: &pb::SelectStmt, is_return: bool, can_set_tag: bool) -> Option<QueryOut> {
        if s.into_clause.is_some()
            || !s.locking_clause.is_empty()
                && (s.op != pb::SetOperation::SetopNone as i32 || !s.values_lists.is_empty())
        {
            return None;
        }
        let with = s.with_clause.is_some();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
        }
        let r = if s.op != pb::SetOperation::SetopNone as i32 {
            self.set_operation(s, can_set_tag)
        } else if !s.values_lists.is_empty() {
            self.values(s, can_set_tag)
        } else {
            self.simple_select(s, is_return, can_set_tag)
        };
        if with {
            self.b.ctes.pop();
        }
        r
    }

    fn simple_select(
        &mut self,
        s: &pb::SelectStmt,
        is_return: bool,
        can_set_tag: bool,
    ) -> Option<QueryOut> {
        if s.limit_option > pb::LimitOption::Count as i32 || s.group_distinct {
            return None;
        }
        let lvl = self.lvl();
        // Named windows.
        for w in &s.window_clause {
            let NodeEnum::WindowDef(wd) = w.node.as_ref()? else {
                return None;
            };
            self.b
                .level()
                .st
                .named_windows
                .push((wd.name.clone(), (**wd).clone()));
        }
        let fromlist = self.range_table_items(&s.from_clause)?;
        // GROUPING() reads the GROUP BY expressions: bound first for it.
        let uses_grouping = format!("{:?}{:?}{:?}", s.target_list, s.having_clause, s.sort_clause)
            .contains("GroupingFunc");
        if uses_grouping {
            let mut nodes = Vec::new();
            fn plain<'n>(n: &'n Node, out: &mut Vec<&'n Node>) {
                match n.node.as_ref() {
                    Some(NodeEnum::GroupingSet(g)) => g.content.iter().for_each(|c| plain(c, out)),
                    Some(NodeEnum::RowExpr(r)) => r.args.iter().for_each(|c| plain(c, out)),
                    _ => out.push(n),
                }
            }
            s.group_clause.iter().for_each(|g| plain(g, &mut nodes));
            let mut exprs: Vec<Expr> = Vec::new();
            for n in nodes {
                if matches!(n.node.as_ref(), Some(NodeEnum::AConst(_))) {
                    return None;
                }
                let e = self.b.in_clause(Clause::GroupBy, |b| b.bind_expr(n)).ok()?;
                if !exprs.iter().any(|x| x.same_as(&e)) {
                    exprs.push(e);
                }
            }
            self.b.level().st.group_exprs = exprs;
        }
        // The select list.
        let targets = self
            .b
            .in_clause(Clause::Select, |b| b.bind_targets(&s.target_list))
            .ok()?;
        let mut tl: Vec<Tle> = Vec::new();
        for (t, node) in targets.into_iter().zip(target_nodes(&s.target_list)) {
            let e = if matches!(t.expr.ty(), DataType::Unknown) {
                self.b.coerce_implicit(t.expr, &DataType::Text).ok()?
            } else {
                t.expr
            };
            let name = if is_return { None } else { Some(t.name) };
            let _ = node;
            let origin = self.origin_of(&e, lvl);
            tl.push(Tle {
                expr: e,
                name,
                sortref: 0,
                junk: false,
                origin,
                resno: None,
            });
        }
        // WHERE
        let quals = match s.where_clause.as_deref() {
            Some(w) => Some(
                self.b
                    .in_clause(Clause::Where, |b| {
                        let e = b.bind_expr(w)?;
                        b.coerce_bool(e, "WHERE")
                    })
                    .ok()?,
            ),
            None => None,
        };
        let having = match s.having_clause.as_deref() {
            Some(h) => Some(
                self.b
                    .in_clause(Clause::Having, |b| {
                        let e = b.bind_expr(h)?;
                        b.coerce_bool(e, "HAVING")
                    })
                    .ok()?,
            ),
            None => None,
        };
        // ORDER BY
        let mut sorts = Vec::new();
        for n in &s.sort_clause {
            sorts.push(self.sort_item(n, &mut tl, false)?);
        }
        // GROUP BY, with its grouping sets (each a list of the items' sort/group refs).
        let mut acc = GroupAcc {
            groups: Vec::new(),
            exprs: Vec::new(),
            refs: Vec::new(),
        };
        let has_sets = s
            .group_clause
            .iter()
            .any(|g| matches!(g.node.as_ref(), Some(NodeEnum::GroupingSet(_))));
        let mut set_texts = Vec::new();
        // The grouping sets the items expand to (their cross product).
        let mut expanded: Vec<Vec<usize>> = vec![Vec::new()];
        for g in &s.group_clause {
            if has_sets {
                let (text, sets) = self.grouping_set(g, &mut tl, &s.target_list, &sorts, &mut acc)?;
                set_texts.push(text);
                let mut next = Vec::new();
                for a in &expanded {
                    for b in &sets {
                        next.push(a.iter().chain(b).copied().collect::<Vec<_>>());
                    }
                }
                if next.len() > 4096 {
                    return None;
                }
                expanded = next;
            } else {
                self.group_ref(g, &mut tl, &s.target_list, &sorts, &mut acc)?;
            }
        }
        let GroupAcc {
            groups,
            exprs: group_exprs,
            refs: group_refs,
        } = acc;
        // GROUPING()'s arguments and their sort/group refs.
        if uses_grouping {
            let pre = self.b.levels[lvl].st.group_exprs.clone();
            let mut refs = Vec::new();
            for e in &pre {
                let i = group_exprs.iter().position(|x| x.same_as(e))?;
                refs.push(group_refs[i]);
            }
            let q = self.q();
            q.grouping_args = pre;
            q.grouping_refs = refs;
        }
        // With grouping sets, a grouping column some set leaves out can be NULL above the
        // grouping: its Vars carry the GROUP RTE's nulling bit.
        let group_nullable: Vec<bool> = group_refs
            .iter()
            .map(|r| has_sets && !expanded.iter().all(|set| set.contains(r)))
            .collect();
        let grouping_sets = if has_sets { list(&set_texts) } else { "<>".into() };
        // DISTINCT
        let mut distinct: Vec<String> = Vec::new();
        let distinct_all = s.distinct_clause.len() == 1 && s.distinct_clause[0].node.is_none();
        let distinct_on = !s.distinct_clause.is_empty() && !distinct_all;
        if distinct_on {
            // DISTINCT ON: the ORDER BY items it begins with, then its other expressions.
            let mut refs = Vec::new();
            for d in &s.distinct_clause {
                let at = self.target_for(d, &mut tl, Clause::DistinctOn, false)?;
                refs.push(self.sortref(&mut tl, at));
            }
            let mut skipped = false;
            for (sref, text) in &sorts {
                if refs.contains(sref) {
                    if skipped {
                        return None;
                    }
                    distinct.push(text.clone());
                } else {
                    skipped = true;
                }
            }
            for r in refs {
                if sorts.iter().any(|x| x.0 == r) || distinct.iter().any(|d| d.contains(&format!(":tleSortGroupRef {r} "))) {
                    continue;
                }
                let at = tl.iter().position(|t| t.sortref == r)?;
                let (eq, lt, hash) = sort_ops(&tl[at].expr.ty(), false)?;
                distinct.push(format!(
                    "{{SORTGROUPCLAUSE :tleSortGroupRef {r} :eqop {eq} :sortop {lt} :reverse_sort false :nulls_first false :hashable {hash}}}"
                ));
            }
        }
        if distinct_all {
            for (sref, text) in &sorts {
                if tl.iter().any(|t| t.sortref == *sref && t.junk) {
                    return None;
                }
                distinct.push(text.clone());
            }
            for at in 0..tl.len() {
                if tl[at].junk {
                    continue;
                }
                let had = tl[at].sortref;
                if had != 0 && sorts.iter().any(|x| x.0 == had) {
                    continue;
                }
                let ty = tl[at].expr.ty();
                let (eq, lt, hash) = sort_ops(&ty, false)?;
                let sref = self.sortref(&mut tl, at);
                if had != 0 && distinct.iter().any(|d| d.contains(&format!(":tleSortGroupRef {sref} "))) {
                    continue;
                }
                distinct.push(format!(
                    "{{SORTGROUPCLAUSE :tleSortGroupRef {sref} :eqop {eq} :sortop {lt} :reverse_sort false :nulls_first false :hashable {hash}}}"
                ));
            }
        }
        // LIMIT / OFFSET
        let offset = match s.limit_offset.as_deref() {
            Some(n) => {
                let e = self.b.in_clause(Clause::Offset, |b| b.bind_expr(n)).ok()?;
                Some(self.b.coerce_implicit(e, &DataType::Int8).ok()?)
            }
            None => None,
        };
        let limit = match s.limit_count.as_deref() {
            Some(n) => {
                let e = self.b.in_clause(Clause::Limit, |b| b.bind_expr(n)).ok()?;
                Some(self.b.coerce_implicit(e, &DataType::Int8).ok()?)
            }
            None => None,
        };
        // Windows.
        let windows = self.window_clauses(&mut tl)?;
        let st = &self.b.levels[lvl].st;
        let has_aggs = !st.aggs.is_empty() || uses_grouping;
        let has_windows = !st.windows.is_empty();
        let has_srfs = !st.srfs.is_empty();
        // The grouping step's RTE (Postgres 18): what reads the GROUP BY expressions after it
        // reads that entry's columns.
        let grouped = !group_exprs.is_empty();
        if grouped {
            let mut exprs = Vec::new();
            for e in &group_exprs {
                exprs.push(self.expr_text(e)?);
            }
            let names: Vec<String> = group_exprs
                .iter()
                .map(|g| {
                    tl.iter()
                        .find(|t| !t.junk && t.expr.same_as(g))
                        .and_then(|t| t.name.clone())
                        .unwrap_or_else(|| group_colname(g))
                })
                .collect();
            let text = format!(
                "{{RANGETBLENTRY :alias <> :eref {} :rtekind 9 :groupexprs {} :lateral false :inFromCl false :securityQuals <>}}",
                alias_node("*GROUP*", &names),
                list(&exprs)
            );
            let rte = self.add_rte(text);
            let q = self.q();
            q.group = group_exprs.clone();
            q.group_nullable = group_nullable.clone();
            q.group_rte = rte;
            self.subst = Some(lvl);
        }
        // Output.
        let quals_text = match &quals {
            Some(q) => {
                let saved = self.subst.take();
                let r = self.expr_text(q);
                self.subst = saved;
                r?
            }
            None => "<>".into(),
        };
        let (tl_text, cols) = self.target_list(&tl)?;
        let having_text = match &having {
            Some(h) => self.expr_text(h)?,
            None => "<>".into(),
        };
        let offset_text = match &offset {
            Some(e) => self.expr_text(e)?,
            None => "<>".into(),
        };
        let limit_text = match &limit {
            Some(e) => self.expr_text(e)?,
            None => "<>".into(),
        };
        self.subst = None;
        let row_marks = self.row_marks(&s.locking_clause)?;
        let has_for_update = row_marks != "<>";
        let cte_list = self.cte_list();
        let q = self.q();
        let has_sublinks = q.has_sublinks;
        let rtable = list(&q.rtable);
        let perms = perms_text(&q.perms);
        let sorts: Vec<String> = sorts.into_iter().map(|x| x.1).collect();
        let text = format!(
            "{{QUERY :commandType 1 :querySource 0 :canSetTag {can_set_tag} :utilityStmt <> :resultRelation 0 :hasAggs {has_aggs} :hasWindowFuncs {has_windows} :hasTargetSRFs {has_srfs} :hasSubLinks {has_sublinks} :hasDistinctOn {distinct_on} :hasRecursive false :hasModifyingCTE false :hasForUpdate {has_for_update} :hasRowSecurity false :hasGroupRTE {grouped} :isReturn {is_return} :cteList {cte_list} :rtable {rtable} :rteperminfos {perms} :jointree {{FROMEXPR :fromlist {} :quals {quals_text}}} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList {tl_text} :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause {} :groupDistinct false :groupingSets {grouping_sets} :havingQual {having_text} :windowClause {} :distinctClause {} :sortClause {} :limitOffset {offset_text} :limitCount {limit_text} :limitOption {} :rowMarks {row_marks} :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}}",
            list(&fromlist),
            list(&groups),
            list(&windows),
            list(&distinct),
            list(&sorts),
            u8::from(s.limit_option == pb::LimitOption::WithTies as i32),
        );
        let text = self.finish(text);
        Some(QueryOut { text, cols })
    }

    /// FOR UPDATE / SHARE (`transformLockingClause`): each relation it locks (every one in
    /// the range table, or those named) takes a row-share lock and needs UPDATE privilege;
    /// the query's rowMarks list them. Locking a subquery isn't modelled.
    fn row_marks(&mut self, clauses: &[Node]) -> Option<String> {
        let mut marks: Vec<(usize, i32, i32)> = Vec::new();
        for c in clauses {
            let NodeEnum::LockingClause(l) = c.node.as_ref()? else {
                return None;
            };
            let strength = l.strength - 1;
            let wait = l.wait_policy - 1;
            let q = self.q();
            let kind = |t: &str| {
                t.split(" :rtekind ")
                    .nth(1)
                    .and_then(|r| r.split(' ').next())
                    .and_then(|k| k.parse::<i32>().ok())
            };
            let refname = |t: &str| {
                t.split(":eref {ALIAS :aliasname ")
                    .nth(1)
                    .and_then(|r| r.split(' ').next())
                    .map(str::to_string)
            };
            let mut rtis = Vec::new();
            if l.locked_rels.is_empty() {
                for (i, t) in q.rtable.iter().enumerate() {
                    match kind(t)? {
                        0 => rtis.push(i + 1),
                        1 => return None,
                        _ => {}
                    }
                }
            } else {
                for r in &l.locked_rels {
                    let NodeEnum::RangeVar(rv) = r.node.as_ref()? else {
                        return None;
                    };
                    if !rv.schemaname.is_empty() {
                        return None;
                    }
                    let i = q
                        .rtable
                        .iter()
                        .position(|t| refname(t).as_deref() == Some(rv.relname.as_str()))?;
                    if kind(&q.rtable[i])? != 0 {
                        return None;
                    }
                    rtis.push(i + 1);
                }
            }
            for rti in rtis {
                if marks.iter().any(|m| m.0 == rti) {
                    return None;
                }
                let t = &mut q.rtable[rti - 1];
                *t = t.replacen(":rellockmode 1 ", ":rellockmode 2 ", 1);
                let perm: usize = t
                    .split(":perminfoindex ")
                    .nth(1)?
                    .split(' ')
                    .next()?
                    .parse()
                    .ok()?;
                q.perms.get_mut(perm.checked_sub(1)?)?.required |= 4;
                marks.push((rti, strength, wait));
            }
        }
        if marks.is_empty() {
            return Some("<>".into());
        }
        let items: Vec<String> = marks
            .iter()
            .map(|(rti, st, w)| {
                format!("{{ROWMARKCLAUSE :rti {rti} :strength {st} :waitPolicy {w} :pushedDown false}}")
            })
            .collect();
        Some(list(&items))
    }

    /// The query's text with its WITH RECURSIVE flag (the first `hasRecursive` is its own).
    fn finish(&mut self, text: String) -> String {
        if std::mem::take(&mut self.recursive_flag) {
            text.replacen(":hasRecursive false", ":hasRecursive true", 1)
        } else {
            text
        }
    }

    /// The table column a target that is a column comes from (markTargetListOrigins).
    fn origin_of(&self, e: &Expr, lvl: usize) -> (u32, i32) {
        match e {
            Expr::Col(i, _) => self
                .qs
                .get(&lvl)
                .and_then(|q| q.cols.get(i))
                .map_or((0, 0), |v| v.origin),
            Expr::Outer(1, i, _) => match self.b.levels.get(lvl).and_then(|l| l.params.get(*i)) {
                Some(p @ Expr::Col(..)) | Some(p @ Expr::Outer(..)) if lvl > 0 => {
                    self.origin_of(&p.clone(), lvl - 1)
                }
                _ => (0, 0),
            },
            _ => (0, 0),
        }
    }

    /// The TLE's sort/group reference, assigned when it has none.
    fn sortref(&mut self, tl: &mut [Tle], at: usize) -> usize {
        if tl[at].sortref == 0 {
            tl[at].sortref = tl.iter().map(|t| t.sortref).max().unwrap_or(0) + 1;
        }
        tl[at].sortref
    }

    /// A TLE for expression `e`: an equal one, else a new junk one.
    fn find_or_add(&mut self, tl: &mut Vec<Tle>, e: Expr) -> usize {
        let key = format!("{e:?}");
        match tl.iter().position(|t| format!("{:?}", t.expr) == key) {
            Some(i) => i,
            None => {
                tl.push(Tle {
                    expr: e,
                    name: None,
                    sortref: 0,
                    junk: true,
                    origin: (0, 0),
                    resno: None,
                });
                tl.len() - 1
            }
        }
    }

    /// One ORDER BY item (`sql99`: window ORDER BY, by expression only): its reference and
    /// its SortGroupClause.
    fn sort_item(&mut self, n: &Node, tl: &mut Vec<Tle>, sql99: bool) -> Option<(usize, String)> {
        let NodeEnum::SortBy(sb) = n.node.as_ref()? else {
            return None;
        };
        if !sb.use_op.is_empty() {
            return None;
        }
        let desc = sb.sortby_dir == pb::SortByDir::SortbyDesc as i32;
        let nulls_first = match pb::SortByNulls::try_from(sb.sortby_nulls).ok()? {
            pb::SortByNulls::SortbyNullsFirst => true,
            pb::SortByNulls::SortbyNullsLast => false,
            _ => desc,
        };
        let key = sb.node.as_deref()?;
        let clause = if sql99 { Clause::Window } else { Clause::OrderBy };
        let at = self.target_for(key, tl, clause, sql99)?;
        let ty = tl[at].expr.ty();
        let (eq, op, hash) = sort_ops(&ty, desc)?;
        let sref = self.sortref(tl, at);
        Some((
            sref,
            format!(
                "{{SORTGROUPCLAUSE :tleSortGroupRef {sref} :eqop {eq} :sortop {op} :reverse_sort {desc} :nulls_first {nulls_first} :hashable {hash}}}"
            ),
        ))
    }

    /// The TLE an ORDER BY / DISTINCT ON key names (`findTargetlistEntrySQL92`: an output
    /// column's name or position, else an expression, a junk TLE when not in the list).
    fn target_for(&mut self, key: &Node, tl: &mut Vec<Tle>, clause: Clause, sql99: bool) -> Option<usize> {
        if !sql99 {
            match key.node.as_ref() {
                Some(NodeEnum::ColumnRef(cr)) if cr.fields.len() == 1 => {
                    if let Some(i) = ast::sval(&cr.fields[0]).and_then(|n| {
                        tl.iter().position(|t| t.name.as_deref() == Some(n) && !t.junk)
                    }) {
                        return Some(i);
                    }
                }
                Some(NodeEnum::AConst(c)) => {
                    let Some(pb::a_const::Val::Ival(i)) = c.val.as_ref() else {
                        return None;
                    };
                    let i = usize::try_from(i.ival).ok()?.checked_sub(1)?;
                    return (i < tl.len() && !tl[i].junk).then_some(i);
                }
                _ => {}
            }
        }
        let e = self.b.in_clause(clause, |b| b.bind_expr(key)).ok()?;
        let e = if matches!(e.ty(), DataType::Unknown) {
            self.b.coerce_implicit(e, &DataType::Text).ok()?
        } else {
            e
        };
        Some(self.find_or_add(tl, e))
    }

    /// One GROUP BY item: its TLE.
    fn group_item(&mut self, g: &Node, tl: &mut Vec<Tle>, targets: &[Node]) -> Option<usize> {
        // An output column position.
        if let Some(NodeEnum::AConst(c)) = g.node.as_ref() {
            let Some(pb::a_const::Val::Ival(i)) = c.val.as_ref() else {
                return None;
            };
            let i = usize::try_from(i.ival).ok()?.checked_sub(1)?;
            return (i < tl.len() && !tl[i].junk).then_some(i);
        }
        // A bare name: an input column wins over an output name.
        let e = match g.node.as_ref() {
            Some(NodeEnum::ColumnRef(c)) if c.fields.len() == 1 => {
                let name = ast::sval(&c.fields[0])?;
                let input = self
                    .b
                    .in_clause(Clause::GroupBy, |b| b.find_column(None, name, c.location))
                    .ok()?;
                match input {
                    Some(e) => e,
                    None => {
                        let i = targets.iter().position(|t| {
                            matches!(t.node.as_ref(), Some(NodeEnum::ResTarget(r)) if r.name == name)
                        })?;
                        return Some(i);
                    }
                }
            }
            _ => self.b.in_clause(Clause::GroupBy, |b| b.bind_expr(g)).ok()?,
        };
        let e = if matches!(e.ty(), DataType::Unknown) {
            self.b.coerce_implicit(e, &DataType::Text).ok()?
        } else {
            e
        };
        Some(self.find_or_add(tl, e))
    }

    /// A GROUP BY item: its sort/group ref, the expression added to the grouping columns
    /// (with its SortGroupClause) the first time.
    fn group_ref(
        &mut self,
        g: &Node,
        tl: &mut Vec<Tle>,
        targets: &[Node],
        sorts: &[(usize, String)],
        acc: &mut GroupAcc,
    ) -> Option<usize> {
        let at = self.group_item(g, tl, targets)?;
        if let Some(i) = acc.exprs.iter().position(|x| x.same_as(&tl[at].expr)) {
            return Some(acc.refs[i]);
        }
        let ty = tl[at].expr.ty();
        let (eq, lt, hash) = sort_ops(&ty, false)?;
        // A GROUP BY item an ORDER BY item already sorts by takes its ordering.
        let sref = self.sortref(tl, at);
        let sorted = sorts.iter().find(|x| x.0 == sref);
        acc.groups.push(match sorted {
            Some((_, text)) => text.clone(),
            None => format!(
                "{{SORTGROUPCLAUSE :tleSortGroupRef {sref} :eqop {eq} :sortop {lt} :reverse_sort false :nulls_first false :hashable {hash}}}"
            ),
        });
        acc.exprs.push(tl[at].expr.clone());
        acc.refs.push(sref);
        Some(sref)
    }

    /// A GROUP BY item when there are grouping sets: its GroupingSet node and the sets it
    /// expands to.
    fn grouping_set(
        &mut self,
        g: &Node,
        tl: &mut Vec<Tle>,
        targets: &[Node],
        sorts: &[(usize, String)],
        acc: &mut GroupAcc,
    ) -> Option<(String, Vec<Vec<usize>>)> {
        let simple = |refs: &[usize]| {
            format!(
                "{{GROUPINGSET :kind 1 :content {} :location -1}}",
                int_list('i', refs)
            )
        };
        // A plain item or a parenthesized list of them: one set.
        let items = |me: &mut Self, n: &Node, tl: &mut Vec<Tle>, acc: &mut GroupAcc| -> Option<Vec<usize>> {
            match n.node.as_ref()? {
                NodeEnum::RowExpr(r) if r.row_format == pb::CoercionForm::CoerceImplicitCast as i32 => r
                    .args
                    .iter()
                    .map(|a| me.group_ref(a, tl, targets, sorts, acc))
                    .collect(),
                _ => Some(vec![me.group_ref(n, tl, targets, sorts, acc)?]),
            }
        };
        let Some(NodeEnum::GroupingSet(gs)) = g.node.as_ref() else {
            let refs = items(self, g, tl, acc)?;
            return Some((simple(&refs), vec![refs]));
        };
        use pb::GroupingSetKind as K;
        match K::try_from(gs.kind).ok()? {
            K::GroupingSetEmpty => Some((
                "{GROUPINGSET :kind 0 :content <> :location -1}".into(),
                vec![Vec::new()],
            )),
            K::GroupingSetSimple => {
                let mut refs = Vec::new();
                for n in &gs.content {
                    refs.extend(items(self, n, tl, acc)?);
                }
                Some((simple(&refs), vec![refs]))
            }
            k @ (K::GroupingSetRollup | K::GroupingSetCube) => {
                let mut elems = Vec::new();
                for n in &gs.content {
                    elems.push(items(self, n, tl, acc)?);
                }
                let texts: Vec<String> = elems.iter().map(|e| simple(e)).collect();
                let sets: Vec<Vec<usize>> = if k == K::GroupingSetRollup {
                    (0..=elems.len()).rev().map(|n| elems[..n].concat()).collect()
                } else {
                    if elems.len() > 12 {
                        return None;
                    }
                    (0..1usize << elems.len())
                        .map(|m| {
                            (0..elems.len())
                                .filter(|i| m & (1 << i) != 0)
                                .flat_map(|i| elems[i].clone())
                                .collect()
                        })
                        .collect()
                };
                Some((
                    format!(
                        "{{GROUPINGSET :kind {} :content {} :location -1}}",
                        if k == K::GroupingSetRollup { 2 } else { 3 },
                        list(&texts)
                    ),
                    sets,
                ))
            }
            K::GroupingSetSets => {
                let mut texts = Vec::new();
                let mut sets = Vec::new();
                for n in &gs.content {
                    let (t, s) = self.grouping_set(n, tl, targets, sorts, acc)?;
                    texts.push(t);
                    sets.extend(s);
                }
                Some((
                    format!("{{GROUPINGSET :kind 4 :content {} :location -1}}", list(&texts)),
                    sets,
                ))
            }
            K::Undefined => None,
        }
    }

    /// The window clauses the level's window functions use (`p_windowdefs`), each
    /// function's number in `winrefs`.
    fn window_clauses(&mut self, tl: &mut Vec<Tle>) -> Option<Vec<String>> {
        let lvl = self.lvl();
        let st = &self.b.levels[lvl].st;
        if st.windows.is_empty() && st.named_windows.is_empty() {
            return Some(Vec::new());
        }
        let calls: Vec<(String, pb::WindowDef)> = st.window_defs.clone();
        if calls.len() != st.windows.len() {
            return None;
        }
        let mut defs: Vec<pb::WindowDef> =
            st.named_windows.iter().map(|(_, w)| w.clone()).collect();
        let mut winrefs = Vec::new();
        for (_, wd) in &calls {
            if !wd.name.is_empty() {
                let i = defs.iter().position(|d| d.name == wd.name)?;
                winrefs.push(i + 1);
                continue;
            }
            let key = window_key(wd);
            match defs.iter().position(|d| window_key(d) == key && d.refname == wd.refname) {
                Some(i) => winrefs.push(i + 1),
                None => {
                    defs.push(wd.clone());
                    winrefs.push(defs.len());
                }
            }
        }
        self.q().winrefs = winrefs;
        let mut out: Vec<String> = Vec::new();
        let mut clauses: Vec<(String, Vec<String>, Vec<String>)> = Vec::new();
        for (k, wd) in defs.iter().enumerate() {
            let mut order = Vec::new();
            for n in &wd.order_clause {
                order.push(self.sort_item(n, tl, true)?.1);
            }
            let mut partition = Vec::new();
            let mut seen: Vec<usize> = Vec::new();
            for n in &wd.partition_clause {
                let e = self.b.in_clause(Clause::Window, |b| b.bind_expr(n)).ok()?;
                let at = self.find_or_add(tl, e);
                let ty = tl[at].expr.ty();
                let (eq, lt, hash) = sort_ops(&ty, false)?;
                let sref = self.sortref(tl, at);
                if seen.contains(&sref) {
                    continue;
                }
                seen.push(sref);
                let text = order
                    .iter()
                    .find(|o| o.contains(&format!(":tleSortGroupRef {sref} ")))
                    .cloned()
                    .unwrap_or_else(|| format!("{{SORTGROUPCLAUSE :tleSortGroupRef {sref} :eqop {eq} :sortop {lt} :reverse_sort false :nulls_first false :hashable {hash}}}"));
                partition.push(text);
            }
            let (mut partition, mut order, mut copied) = (partition, order, false);
            if !wd.refname.is_empty() {
                let r = clauses.iter().find(|c| c.0 == wd.refname)?;
                partition = r.1.clone();
                if order.is_empty() {
                    order = r.2.clone();
                    copied = true;
                }
            }
            // ROWS / GROUPS offsets are bigints. RANGE ones compare with the one ORDER BY
            // column through an in_range support function of its btree opfamily, the
            // offset coerced to that function's type (transformFrameOffset).
            let mut offsets = Vec::new();
            let mut in_range = [0u32; 2];
            let mut range_key: Option<(u32, bool, bool)> = None;
            for (slot, o) in [&wd.start_offset, &wd.end_offset].into_iter().enumerate() {
                offsets.push(match o.as_deref() {
                    Some(n) => {
                        let e = self.b.in_clause(Clause::Window, |b| b.bind_expr(n)).ok()?;
                        let e = if wd.frame_options & 0x2 != 0 {
                            let [key] = order.as_slice() else {
                                return None;
                            };
                            let sref: usize = key
                                .split(":tleSortGroupRef ")
                                .nth(1)?
                                .split(' ')
                                .next()?
                                .parse()
                                .ok()?;
                            let col = tl.iter().find(|t| t.sortref == sref)?.expr.clone();
                            let cty = col.ty().oid();
                            let ety = e.ty();
                            let preferred = if ety == DataType::Unknown { cty } else { ety.oid() };
                            let mut chosen: Option<(u32, u32)> = None;
                            for p in PROCS.iter().filter(|p| p.1 == "in_range" && p.2.len() == 5 && p.2[0] == cty && p.2[1] == cty) {
                                let rt = p.2[2];
                                let ok = ety == DataType::Unknown
                                    || ety.oid() == rt
                                    || DataType::from_oid(rt).is_some_and(|t| self.b.coerce_implicit(e.clone(), &t).is_ok());
                                if !ok {
                                    continue;
                                }
                                if chosen.is_none_or(|c| c.0 != preferred && rt == preferred) {
                                    chosen = Some((rt, p.0));
                                }
                            }
                            let (rt, func) = chosen?;
                            in_range[slot] = func;
                            range_key = Some((
                                self.collations(&[&col], &col.ty()).0,
                                !key.contains(":reverse_sort true"),
                                key.contains(":nulls_first true"),
                            ));
                            self.b.coerce_implicit(e, &DataType::from_oid(rt)?).ok()?
                        } else {
                            self.b.coerce_implicit(e, &DataType::Int8).ok()?
                        };
                        self.expr_text(&e)?
                    }
                    None => "<>".into(),
                });
            }
            let (in_coll, in_asc, in_nulls_first) = range_key.unwrap_or((0, false, false));
            let mut name = String::new();
            token(&mut name, &wd.name);
            let mut refname = String::new();
            token(&mut refname, &wd.refname);
            out.push(format!(
                "{{WINDOWCLAUSE :name {name} :refname {refname} :partitionClause {} :orderClause {} :frameOptions {} :startOffset {} :endOffset {} :startInRangeFunc {} :endInRangeFunc {} :inRangeColl {in_coll} :inRangeAsc {in_asc} :inRangeNullsFirst {in_nulls_first} :winref {} :copiedOrder {copied}}}",
                list(&partition),
                list(&order),
                wd.frame_options,
                offsets[0],
                offsets[1],
                in_range[0],
                in_range[1],
                k + 1
            ));
            clauses.push((wd.name.clone(), partition, order));
        }
        Some(out)
    }

    /// The target list's text, and the output columns.
    fn target_list(&mut self, tl: &[Tle]) -> Option<(String, Vec<OutInfo>)> {
        let mut items = Vec::new();
        let mut cols = Vec::new();
        // A view's query: its output columns take the view's names.
        let renamed: Vec<Tle>;
        let tl = match self.rename.as_ref().filter(|_| self.lvl() == 1) {
            Some(names) => {
                let mut names = names.iter();
                renamed = tl
                    .iter()
                    .map(|t| Tle {
                        expr: t.expr.clone(),
                        name: if t.junk {
                            t.name.clone()
                        } else {
                            names.next().cloned().or_else(|| t.name.clone())
                        },
                        sortref: t.sortref,
                        junk: t.junk,
                        origin: t.origin,
                        resno: t.resno,
                    })
                    .collect();
                &renamed[..]
            }
            None => tl,
        };
        for (i, t) in tl.iter().enumerate() {
            let mut out = String::from("{TARGETENTRY :expr ");
            self.expr(&mut out, &t.expr, self.lvl())?;
            let resno = t.resno.unwrap_or(i as i32 + 1);
            let _ = write!(out, " :resno {resno} :resname ");
            match &t.name {
                Some(n) => token(&mut out, n),
                None => out.push_str("<>"),
            }
            let _ = write!(
                out,
                " :ressortgroupref {} :resorigtbl {} :resorigcol {} :resjunk {}}}",
                t.sortref, t.origin.0, t.origin.1, t.junk
            );
            items.push(out);
            if !t.junk {
                let ty = t.expr.ty();
                let collation = self.b.output_collation(&t.expr).ok()?;
                let coll = if ty.collatable() {
                    collation.as_ref().map_or_else(|| type_collation(&ty), coll_oid)
                } else {
                    0
                };
                cols.push(OutInfo {
                    name: t.name.clone().unwrap_or_else(|| "?column?".into()),
                    typmod: expr_typmod(&t.expr),
                    ty,
                    coll,
                    collation,
                    origin: t.origin,
                });
            }
        }
        Some((list(&items), cols))
    }

    /// `VALUES (...), (...)` as a query over its RTE_VALUES.
    fn values(&mut self, s: &pb::SelectStmt, can_set_tag: bool) -> Option<QueryOut> {
        if !s.sort_clause.is_empty() || s.limit_count.is_some() || s.limit_offset.is_some() {
            return None;
        }
        let (lists, infos) = self.values_lists(&s.values_lists, None)?;
        let names: Vec<String> = (1..=infos.len()).map(|i| format!("column{i}")).collect();
        let text = format!(
            "{{RANGETBLENTRY :alias <> :eref {} :rtekind 5 :values_lists {} :coltypes {} :coltypmods {} :colcollations {} :lateral false :inFromCl true :securityQuals <>}}",
            alias_node("*VALUES*", &names),
            list(&lists),
            int_list('o', &infos.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
            int_list('i', &infos.iter().map(|c| c.typmod).collect::<Vec<_>>()),
            int_list('o', &infos.iter().map(|c| c.coll).collect::<Vec<_>>()),
        );
        let rte = self.add_rte(text);
        let mut infos = infos;
        for (c, n) in infos.iter_mut().zip(&names) {
            c.name = n.clone();
        }
        let (rel, ns) = self.derived_item("*VALUES*", rte, None, &infos);
        self.b.level().scope.rels.push(rel);
        let mut tl = Vec::new();
        for (c, info) in ns.iter().zip(&infos) {
            tl.push(Tle {
                expr: self.col_expr(c.col)?,
                name: Some(info.name.clone()),
                sortref: 0,
                junk: false,
                origin: (0, 0),
                resno: None,
            });
        }
        let (tl_text, cols) = self.target_list(&tl)?;
        let cte_list = self.cte_list();
        let q = self.q();
        let has_sublinks = q.has_sublinks;
        let text = format!(
            "{{QUERY :commandType 1 :querySource 0 :canSetTag {can_set_tag} :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks {has_sublinks} :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList {cte_list} :rtable {} :rteperminfos {} :jointree {{FROMEXPR :fromlist ({{RANGETBLREF :rtindex {rte}}}) :quals <>}} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList {tl_text} :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}}",
            list(&q.rtable),
            perms_text(&q.perms),
        );
        let text = self.finish(text);
        Some(QueryOut { text, cols })
    }

    /// VALUES lists' texts, each column coerced to its common type (or `to`), and the
    /// columns.
    fn values_lists(
        &mut self,
        lists: &[Node],
        to: Option<&[DataType]>,
    ) -> Option<(Vec<String>, Vec<OutInfo>)> {
        let mut rows: Vec<Vec<Expr>> = Vec::new();
        for l in lists {
            let NodeEnum::List(items) = l.node.as_ref()? else {
                return None;
            };
            let row = self
                .b
                .in_clause(Clause::Values, |b| {
                    items.items.iter().map(|i| b.bind_expr(i)).collect::<crate::error::Result<Vec<_>>>()
                })
                .ok()?;
            if rows.first().is_some_and(|f| f.len() != row.len()) {
                return None;
            }
            rows.push(row);
        }
        let ncols = rows.first()?.len();
        let mut infos = Vec::new();
        for c in 0..ncols {
            let ty = match to {
                Some(t) => t.get(c)?.clone(),
                None => {
                    let col: Vec<DataType> = rows.iter().map(|r| r[c].ty()).collect();
                    let t = crate::sql::binder::unify_types(&col, "VALUES").ok()?;
                    if matches!(t, DataType::Unknown) { DataType::Text } else { t }
                }
            };
            for r in &mut rows {
                if matches!(r[c], Expr::Default(_)) {
                    return None;
                }
                let e = std::mem::replace(&mut r[c], Expr::Const(Value::Null, DataType::Unknown));
                r[c] = match to {
                    Some(_) => self.b.coerce_assign(e, &ty, "").ok()?,
                    None => self.b.coerce_implicit(e, &ty).ok()?,
                };
            }
            // (INSERT's VALUES are only assigned: no collations.)
            let refs: Vec<&Expr> = rows.iter().map(|r| &r[c]).collect();
            let coll = if to.is_some() { 0 } else { self.collations(&refs, &ty).1 };
            let typmod = {
                let mods: Vec<i32> = rows.iter().map(|r| expr_typmod(&r[c])).collect();
                if mods.iter().all(|m| *m == mods[0]) { mods[0] } else { -1 }
            };
            infos.push(OutInfo {
                name: format!("column{}", c + 1),
                ty: ty.clone(),
                typmod,
                coll,
                collation: None,
                origin: (0, 0),
            });
        }
        let mut texts = Vec::new();
        for r in &rows {
            let refs: Vec<&Expr> = r.iter().collect();
            let mut out = String::new();
            let lvl = self.lvl();
            self.list_into(&mut out, &refs, lvl)?;
            texts.push(out);
        }
        Some((texts, infos))
    }

    /// UNION / INTERSECT / EXCEPT: the leaves as subqueries, and the tree over them.
    fn set_operation(&mut self, s: &pb::SelectStmt, can_set_tag: bool) -> Option<QueryOut> {
        let mut leaves: Vec<QueryOut> = Vec::new();
        let tree = self.setop_tree(s, &mut leaves)?;
        let (tree_text, types) = tree;
        let leftmost = leaves.first()?;
        let mut tl = Vec::new();
        let names: Vec<String> = leftmost.cols.iter().map(|c| c.name.clone()).collect();
        // The leaves' range-table entries.
        for (i, leaf) in leaves.iter().enumerate() {
            let name = format!("*SELECT* {}", i + 1);
            let cols: Vec<String> = leaf.cols.iter().map(|c| c.name.clone()).collect();
            let text = format!(
                "{{RANGETBLENTRY :alias {} :eref {} :rtekind 1 :subquery {} :security_barrier false :relid 0 :inh false :relkind <> :rellockmode 0 :perminfoindex 0 :lateral false :inFromCl false :securityQuals <>}}",
                alias_node(&name, &[]),
                alias_node(&name, &cols),
                leaf.text
            );
            self.add_rte(text);
        }
        let infos: Vec<OutInfo> = types
            .iter()
            .zip(&names)
            .map(|(t, n)| OutInfo {
                name: n.clone(),
                ..t.clone()
            })
            .collect();
        let (rel, ns) = self.derived_item("", 1, None, &infos);
        self.b.level().scope.rels.push(rel);
        for (c, info) in ns.iter().zip(&infos) {
            tl.push(Tle {
                expr: self.col_expr(c.col)?,
                name: Some(info.name.clone()),
                sortref: 0,
                junk: false,
                origin: (0, 0),
                resno: None,
            });
        }
        // ORDER BY / LIMIT over the result columns.
        let mut sorts = Vec::new();
        for n in &s.sort_clause {
            sorts.push(self.sort_item(n, &mut tl, false)?.1);
        }
        if tl.iter().any(|t| t.junk) || s.limit_option == pb::LimitOption::WithTies as i32 {
            return None;
        }
        let mut limits = Vec::new();
        for (n, clause) in [(&s.limit_offset, Clause::Offset), (&s.limit_count, Clause::Limit)] {
            limits.push(match n.as_deref() {
                Some(n) => {
                    let e = self.b.in_clause(clause, |b| b.bind_expr(n)).ok()?;
                    let e = self.b.coerce_implicit(e, &DataType::Int8).ok()?;
                    self.expr_text(&e)?
                }
                None => "<>".into(),
            });
        }
        let (tl_text, cols) = self.target_list(&tl)?;
        let cte_list = self.cte_list();
        let q = self.q();
        let text = format!(
            "{{QUERY :commandType 1 :querySource 0 :canSetTag {can_set_tag} :utilityStmt <> :resultRelation 0 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks {} :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList {cte_list} :rtable {} :rteperminfos <> :jointree {{FROMEXPR :fromlist <> :quals <>}} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList {tl_text} :override 0 :onConflict <> :returningOldAlias <> :returningNewAlias <> :returningList <> :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause {} :limitOffset {} :limitCount {} :limitOption 0 :rowMarks <> :setOperations {tree_text} :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}}",
            q.has_sublinks,
            list(&q.rtable),
            list(&sorts),
            limits[0],
            limits[1],
        );
        let text = self.finish(text);
        Some(QueryOut { text, cols })
    }

    /// A set operation's tree: its text and result columns (type, typmod, collation).
    fn setop_tree(
        &mut self,
        s: &pb::SelectStmt,
        leaves: &mut Vec<QueryOut>,
    ) -> Option<(String, Vec<OutInfo>)> {
        let op = pb::SetOperation::try_from(s.op).ok()?;
        if op == pb::SetOperation::SetopNone {
            if s.with_clause.is_some() || !s.sort_clause.is_empty() {
                return None;
            }
            let out = self.nested(|t| t.select(s, false, true))?;
            let cols = out.cols.clone();
            leaves.push(out);
            return Some((format!("{{RANGETBLREF :rtindex {}}}", leaves.len()), cols));
        }
        let (l, lcols) = self.setop_tree(s.larg.as_deref()?, leaves)?;
        let (r, rcols) = self.setop_tree(s.rarg.as_deref()?, leaves)?;
        if lcols.len() != rcols.len() {
            return None;
        }
        let code = match op {
            pb::SetOperation::SetopUnion => 1,
            pb::SetOperation::SetopIntersect => 2,
            _ => 3,
        };
        let mut cols = Vec::new();
        let mut groups = Vec::new();
        for (lc, rc) in lcols.iter().zip(&rcols) {
            let ty = crate::sql::binder::unify_types(&[lc.ty.clone(), rc.ty.clone()], "UNION").ok()?;
            let typmod = if lc.typmod == rc.typmod { lc.typmod } else { -1 };
            let coll = if lc.coll == rc.coll { lc.coll } else { return None };
            if !(code == 1 && s.all) {
                let (eq, lt, hash) = sort_ops(&ty, false)?;
                groups.push(format!("{{SORTGROUPCLAUSE :tleSortGroupRef 0 :eqop {eq} :sortop {lt} :reverse_sort false :nulls_first false :hashable {hash}}}"));
            }
            cols.push(OutInfo {
                name: lc.name.clone(),
                ty,
                typmod,
                coll,
                collation: lc.collation.clone(),
                origin: (0, 0),
            });
        }
        let text = format!(
            "{{SETOPERATIONSTMT :op {code} :all {} :larg {l} :rarg {r} :colTypes {} :colTypmods {} :colCollations {} :groupClauses {}}}",
            s.all,
            int_list('o', &cols.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
            int_list('i', &cols.iter().map(|c| c.typmod).collect::<Vec<_>>()),
            int_list('o', &cols.iter().map(|c| c.coll).collect::<Vec<_>>()),
            list(&groups),
        );
        Some((text, cols))
    }

    // ── INSERT / UPDATE / DELETE ──

    /// The target table of a data-modifying statement.
    fn target_table(&mut self, rv: &pb::RangeVar) -> Option<Arc<Table>> {
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        match self.b.cat.find(schema, &rv.relname, &self.b.ctx.search_path)? {
            Relation::Table(t) if t.partkey.is_none() => Some(t),
            _ => None,
        }
    }

    /// The target as range-table entry 1, its scope added.
    fn result_relation(
        &mut self,
        t: &Arc<Table>,
        rv: &pb::RangeVar,
        inh: bool,
        required: u32,
    ) -> Option<(usize, Vec<NsCol>)> {
        let names: Vec<String> = t
            .columns
            .iter()
            .map(|c| if c.dropped { String::new() } else { c.name.clone() })
            .collect();
        let alias = rv.alias.as_ref();
        let refname = alias.map_or(t.name.clone(), |a| a.aliasname.clone());
        let (rte, perm) =
            self.relation_rte(t.oid, 'r', alias, &refname, &names, inh, 3, required, false);
        let schema = alias.is_none().then(|| self.b.cat.schema_name(t.schema).to_string());
        let (rel, ns) = self.table_item(t, rte, perm, &refname, schema, &[])?;
        self.b.level().scope.rels.push(rel);
        Some((perm, ns))
    }

    fn returning(&mut self, list_: &[Node]) -> Option<Vec<Tle>> {
        let targets = self
            .b
            .in_clause(Clause::Returning, |b| b.bind_targets(list_))
            .ok()?;
        let mut tl = Vec::new();
        for t in targets {
            let e = if matches!(t.expr.ty(), DataType::Unknown) {
                self.b.coerce_implicit(t.expr, &DataType::Text).ok()?
            } else {
                t.expr
            };
            tl.push(Tle {
                expr: e,
                name: Some(t.name),
                sortref: 0,
                junk: false,
                origin: (t.table, t.attnum as i32),
                resno: None,
            });
        }
        Some(tl)
    }

    fn dml_query(&mut self, d: Dml) -> Option<QueryOut> {
        let cte_list = self.cte_list();
        let q = self.q();
        let has_sublinks = q.has_sublinks;
        let text = format!(
            "{{QUERY :commandType {} :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 1 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks {has_sublinks} :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList {cte_list} :rtable {} :rteperminfos {} :jointree {{FROMEXPR :fromlist {} :quals {}}} :mergeActionList <> :mergeTargetRelation 0 :mergeJoinCondition <> :targetList {} :override {} :onConflict {} :returningOldAlias {} :returningNewAlias {} :returningList {} :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}}",
            d.command,
            list(&q.rtable),
            perms_text(&q.perms),
            list(&d.fromlist),
            d.quals,
            d.targets,
            d.overriding,
            d.on_conflict,
            if d.returning.is_some() { "old" } else { "<>" },
            if d.returning.is_some() { "new" } else { "<>" },
            d.returning.as_deref().unwrap_or("<>"),
        );
        Some(QueryOut {
            text: self.finish(text),
            cols: d.cols,
        })
    }

    fn insert(&mut self, s: &pb::InsertStmt) -> Option<QueryOut> {
        let with = s.with_clause.is_some();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
        }
        let rv = s.relation.as_ref()?;
        let t = self.target_table(rv)?;
        // The target columns: those listed, else all; and the subscripts of those that
        // assign an element.
        let mut cols: Vec<usize> = Vec::new();
        let mut paths: Vec<(&[Node], i32)> = Vec::new();
        for c in &s.cols {
            let NodeEnum::ResTarget(rt) = c.node.as_ref()? else {
                return None;
            };
            if !rt
                .indirection
                .iter()
                .all(|i| matches!(i.node.as_ref(), Some(NodeEnum::AIndices(_))))
            {
                return None;
            }
            let col = t.columns.iter().position(|c| !c.dropped && c.name == rt.name)?;
            if cols.contains(&col) {
                return None;
            }
            cols.push(col);
            paths.push((&rt.indirection, rt.location));
        }
        let indirect = paths.iter().any(|p| !p.0.is_empty());
        let explicit = !cols.is_empty();
        if !explicit {
            cols = (0..t.columns.len()).filter(|&i| !t.columns[i].dropped).collect();
        }
        let tys: Vec<DataType> = cols.iter().map(|&i| t.columns[i].ty.clone()).collect();
        // The source, as expressions for the columns.
        let mut fromlist = Vec::new();
        let mut exprs: Vec<Expr> = Vec::new();
        let select = s.select_stmt.as_deref();
        let source = select.and_then(|n| match n.node.as_ref() {
            Some(NodeEnum::SelectStmt(sel)) => Some(sel),
            _ => None,
        });
        let single = source.filter(|sel| {
            sel.values_lists.len() == 1
                && sel.with_clause.is_none()
                && sel.sort_clause.is_empty()
                && sel.limit_count.is_none()
        });
        // The target is the first entry; a SELECT's or several VALUES rows' is the second.
        let (perm, _ns) = {
            let saved = std::mem::take(&mut self.b.level().scope);
            let r = self.result_relation(&t, rv, false, 1);
            // Only RETURNING and ON CONFLICT see the target.
            let target_scope = std::mem::replace(&mut self.b.level().scope, saved);
            (r?, target_scope)
        };
        let target_scope = _ns;
        let (perm, _target_cols) = perm;
        match (source, single) {
            (None, _) => {
                // DEFAULT VALUES
                if !explicit {
                    cols.clear();
                }
            }
            (Some(_), Some(sel)) => {
                let NodeEnum::List(items) = sel.values_lists[0].node.as_ref()? else {
                    return None;
                };
                if items.items.len() > cols.len() {
                    return None;
                }
                for (k, item) in items.items.iter().enumerate() {
                    let path = paths.get(k).map_or(&[][..], |p| p.0);
                    if matches!(item.node.as_ref(), Some(NodeEnum::SetToDefault(_))) {
                        if !path.is_empty() {
                            return None;
                        }
                        exprs.push(set_to_default(&t.columns[cols[k]]));
                        continue;
                    }
                    let e = self
                        .b
                        .in_clause(Clause::ValuesSingle, |b| b.bind_expr(item))
                        .ok()?;
                    let name = &t.columns[cols[k]].name;
                    let e = if path.is_empty() {
                        self.b.coerce_assign(e, &tys[k], name).ok()?
                    } else {
                        // An element of a new row's NULL array.
                        let base = Expr::Const(Value::Null, tys[k].clone());
                        self.b
                            .in_clause(Clause::ValuesSingle, |b| {
                                b.assign_path(base, path, e, name, paths[k].1)
                            })
                            .ok()?
                    };
                    exprs.push(e);
                }
                cols.truncate(exprs.len());
            }
            (Some(_), None) if indirect => return None,
            (Some(sel), None) if !sel.values_lists.is_empty() && sel.with_clause.is_none() => {
                let (lists, infos) = self.values_lists(&sel.values_lists, Some(&tys))?;
                let names: Vec<String> = (1..=infos.len()).map(|i| format!("column{i}")).collect();
                let text = format!(
                    "{{RANGETBLENTRY :alias <> :eref {} :rtekind 5 :values_lists {} :coltypes {} :coltypmods {} :colcollations {} :lateral false :inFromCl true :securityQuals <>}}",
                    alias_node("*VALUES*", &names),
                    list(&lists),
                    int_list('o', &infos.iter().map(|c| c.ty.oid()).collect::<Vec<_>>()),
                    int_list('i', &infos.iter().map(|c| c.typmod).collect::<Vec<_>>()),
                    int_list('o', &infos.iter().map(|c| c.coll).collect::<Vec<_>>()),
                );
                let rte = self.add_rte(text);
                fromlist.push(format!("{{RANGETBLREF :rtindex {rte}}}"));
                let (_, ns) = self.derived_item("*VALUES*", rte, None, &infos);
                for c in &ns {
                    exprs.push(self.col_expr(c.col)?);
                }
                cols.truncate(exprs.len());
            }
            (Some(sel), None) => {
                let out = self.nested(|t| t.select(sel, false, true))?;
                if out.cols.len() > cols.len() {
                    return None;
                }
                let names: Vec<String> = out.cols.iter().map(|c| c.name.clone()).collect();
                let text = format!(
                    "{{RANGETBLENTRY :alias {} :eref {} :rtekind 1 :subquery {} :security_barrier false :relid 0 :inh false :relkind <> :rellockmode 0 :perminfoindex 0 :lateral false :inFromCl false :securityQuals <>}}",
                    alias_node("*SELECT*", &[]),
                    alias_node("*SELECT*", &names),
                    out.text
                );
                let rte = self.add_rte(text);
                fromlist.push(format!("{{RANGETBLREF :rtindex {rte}}}"));
                let (_, ns) = self.derived_item("*SELECT*", rte, None, &out.cols);
                for (k, c) in ns.iter().enumerate() {
                    let e = self.col_expr(c.col)?;
                    exprs.push(self.b.coerce_assign(e, &tys[k], &t.columns[cols[k]].name).ok()?);
                }
                cols.truncate(exprs.len());
            }
        }
        let mut tl = Vec::new();
        for (e, &c) in exprs.into_iter().zip(&cols) {
            tl.push(Tle {
                expr: e,
                name: Some(t.columns[c].name.clone()),
                sortref: 0,
                junk: false,
                origin: (0, 0),
                resno: Some(c as i32 + 1),
            });
            self.q().perms[perm].inserted.insert(c as i32 + 1 + 7);
        }
        let (targets, _) = self.target_list(&tl)?;
        // RETURNING and ON CONFLICT read the target.
        self.b.level().scope = target_scope;
        let on_conflict = match &s.on_conflict_clause {
            Some(oc) => self.on_conflict(oc, &t, perm)?,
            None => "<>".into(),
        };
        let returning = match &s.returning_clause {
            Some(r) if !r.exprs.is_empty() => {
                if r.options.iter().any(|_| true) {
                    return None;
                }
                let tl = self.returning(&r.exprs)?;
                Some(self.target_list(&tl)?)
            }
            _ => None,
        };
        let r = self.dml_query(Dml {
            command: 3,
            overriding: (s.r#override - 1).max(0),
            fromlist,
            quals: "<>".into(),
            targets,
            on_conflict,
            cols: returning.as_ref().map(|r| r.1.clone()).unwrap_or_default(),
            returning: returning.map(|r| r.0),
        });
        if with {
            self.b.ctes.pop();
        }
        r
    }

    fn on_conflict(&mut self, oc: &pb::OnConflictClause, t: &Arc<Table>, perm: usize) -> Option<String> {
        let action = pb::OnConflictAction::try_from(oc.action).ok()?;
        // The arbiter: index columns as written.
        let mut arbiters = Vec::new();
        if let Some(inf) = &oc.infer {
            if !inf.conname.is_empty() || inf.where_clause.is_some() {
                return None;
            }
            for e in &inf.index_elems {
                let NodeEnum::IndexElem(ie) = e.node.as_ref()? else {
                    return None;
                };
                if ie.name.is_empty() || ie.collation.len() + ie.opclass.len() > 0 {
                    return None;
                }
                let i = t.columns.iter().position(|c| !c.dropped && c.name == ie.name)?;
                let e = Expr::Col(i, t.columns[i].ty.clone());
                arbiters.push(format!(
                    "{{INFERENCEELEM :expr {} :infercollid 0 :inferopclass 0}}",
                    self.expr_text(&e)?
                ));
            }
        }
        if action == pb::OnConflictAction::OnconflictNothing {
            return Some(format!(
                "{{ONCONFLICTEXPR :action 1 :arbiterElems {} :arbiterWhere <> :constraint 0 :onConflictSet <> :onConflictWhere <> :exclRelIndex 0 :exclRelTlist <>}}",
                list(&arbiters)
            ));
        }
        // DO UPDATE: EXCLUDED as entry 2.
        let names: Vec<String> = t
            .columns
            .iter()
            .map(|c| if c.dropped { String::new() } else { c.name.clone() })
            .collect();
        let excl_alias = pb::Alias {
            aliasname: "excluded".into(),
            colnames: vec![],
        };
        let (rte, eperm) =
            self.relation_rte(t.oid, 'c', Some(&excl_alias), "excluded", &names, false, 3, 2, false);
        // (EXCLUDED's permission entry asks for nothing until read.)
        self.q().perms[eperm].required = 0;
        let (rel, _) = self.table_item(t, rte, eperm, "excluded", None, &[])?;
        self.b.level().scope.rels.push(rel);
        let mut sets = Vec::new();
        for n in &oc.target_list {
            let NodeEnum::ResTarget(rt) = n.node.as_ref()? else {
                return None;
            };
            if !rt.indirection.is_empty() {
                return None;
            }
            let c = t.columns.iter().position(|c| !c.dropped && c.name == rt.name)?;
            let e = self
                .b
                .in_clause(Clause::UpdateSet, |b| b.bind_expr(rt.val.as_deref().unwrap()))
                .ok()?;
            let e = self.b.coerce_assign(e, &t.columns[c].ty, &rt.name).ok()?;
            self.q().perms[perm].updated.insert(c as i32 + 1 + 7);
            sets.push(Tle {
                expr: e,
                name: Some(rt.name.clone()),
                sortref: 0,
                junk: false,
                origin: (0, 0),
                resno: Some(c as i32 + 1),
            });
        }
        let (set_text, _) = self.target_list(&sets)?;
        let where_text = match oc.where_clause.as_deref() {
            Some(w) => {
                let e = self
                    .b
                    .in_clause(Clause::Where, |b| {
                        let e = b.bind_expr(w)?;
                        b.coerce_bool(e, "WHERE")
                    })
                    .ok()?;
                self.expr_text(&e)?
            }
            None => "<>".into(),
        };
        if self.q().perms[eperm].required != 0 {
            self.q().perms[eperm].required = 2;
        }
        self.q().perms[perm].required |= 4;
        // EXCLUDED's columns, and a whole-row one.
        let mut excl = Vec::new();
        for (i, c) in t.columns.iter().enumerate() {
            if c.dropped {
                continue;
            }
            let _ = write!(
                excl.push_get(),
                "{{TARGETENTRY :expr {{VAR :varno {rte} :varattno {0} :vartype {1} :vartypmod {2} :varcollid {3} :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn {rte} :varattnosyn {0} :location -1}} :resno {0} :resname ",
                i + 1,
                c.domain.unwrap_or_else(|| c.ty.oid()),
                c.ty.typmod(),
                crate::catalog::system::column_collation(c),
            );
            let last = excl.last_mut()?;
            token(last, &c.name);
            last.push_str(" :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk false}");
        }
        excl.push(format!(
            "{{TARGETENTRY :expr {{VAR :varno {rte} :varattno 0 :vartype {0} :vartypmod -1 :varcollid 0 :varnullingrels (b) :varlevelsup 0 :varreturningtype 0 :varnosyn {rte} :varattnosyn 0 :location -1}} :resno 0 :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk true}}",
            t.rowtype
        ));
        Some(format!(
            "{{ONCONFLICTEXPR :action 2 :arbiterElems {} :arbiterWhere <> :constraint 0 :onConflictSet {set_text} :onConflictWhere {where_text} :exclRelIndex {rte} :exclRelTlist {}}}",
            list(&arbiters),
            list(&excl)
        ))
    }

    fn update(&mut self, s: &pb::UpdateStmt) -> Option<QueryOut> {
        let with = s.with_clause.is_some();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
        }
        let rv = s.relation.as_ref()?;
        let t = self.target_table(rv)?;
        let (perm, _) = self.result_relation(&t, rv, rv.inh, 4)?;
        let mut fromlist = vec!["{RANGETBLREF :rtindex 1}".to_string()];
        fromlist.extend(self.range_table_items(&s.from_clause)?);
        let mut tl = Vec::new();
        // `(a, b) = (SELECT ...)` sublinks (MULTIEXPR): their texts, in order.
        let mut multi: Vec<String> = Vec::new();
        let mut multi_cols: Vec<OutInfo> = Vec::new();
        for n in &s.target_list {
            let NodeEnum::ResTarget(rt) = n.node.as_ref()? else {
                return None;
            };
            let c = t.columns.iter().position(|c| !c.dropped && c.name == rt.name)?;
            let mut val = rt.val.as_deref()?;
            // `(a, b) = (SELECT ...)`: the first column analyzes the sublink; each column
            // reads its output column as a MULTIEXPR param.
            if let Some(NodeEnum::MultiAssignRef(m)) = val.node.as_ref()
                && let Some(NodeEnum::SubLink(sl)) = m.source.as_deref()?.node.as_ref()
            {
                if !rt.indirection.is_empty() {
                    return None;
                }
                if m.colno == 1 {
                    let NodeEnum::SelectStmt(sel) = sl.subselect.as_deref()?.node.as_ref()? else {
                        return None;
                    };
                    let q = self.nested(|t| t.select(sel, false, true))?;
                    if q.cols.len() as i32 != m.ncolumns {
                        return None;
                    }
                    multi.push(q.text);
                    multi_cols = q.cols;
                }
                let col = multi_cols.get(usize::try_from(m.colno - 1).ok()?)?;
                let id = (multi.len() << 16) | m.colno as usize;
                let p = Expr::Param(id - 1, col.ty.clone());
                let e = self.b.coerce_assign(p, &t.columns[c].ty, &rt.name).ok()?;
                self.q().perms[perm].updated.insert(c as i32 + 1 + 7);
                tl.push(Tle {
                    expr: e,
                    name: Some(rt.name.clone()),
                    sortref: 0,
                    junk: false,
                    origin: (0, 0),
                    resno: Some(c as i32 + 1),
                });
                continue;
            }
            // `(a, b) = ROW(...)`: each column takes its item (transformMultiAssignRef).
            if let Some(NodeEnum::MultiAssignRef(m)) = val.node.as_ref() {
                let Some(NodeEnum::RowExpr(r)) = m.source.as_deref()?.node.as_ref() else {
                    return None;
                };
                if r.args.len() as i32 != m.ncolumns {
                    return None;
                }
                val = r.args.get(usize::try_from(m.colno - 1).ok()?)?;
            }
            let e = if !rt.indirection.is_empty() {
                // An element or slice: the column's value with it replaced (the column is
                // read without being marked selected).
                if !rt
                    .indirection
                    .iter()
                    .all(|i| matches!(i.node.as_ref(), Some(NodeEnum::AIndices(_))))
                    || matches!(val.node.as_ref(), Some(NodeEnum::SetToDefault(_)))
                {
                    return None;
                }
                let Expr::Col(col, ty) = self.b.level().scope.rels[0].cols.get(c)?.expr.clone() else {
                    return None;
                };
                let v = VarRef {
                    perm: None,
                    ..self.q().cols.get(&col)?.clone()
                };
                let base = Expr::Col(self.new_col(v), ty);
                let name = rt.name.clone();
                self.b
                    .in_clause(Clause::UpdateSet, |b| {
                        let e = b.bind_expr(val)?;
                        b.assign_path(base, &rt.indirection, e, &name, rt.location)
                    })
                    .ok()?
            } else if matches!(val.node.as_ref(), Some(NodeEnum::SetToDefault(_))) {
                set_to_default(&t.columns[c])
            } else {
                let e = self
                    .b
                    .in_clause(Clause::UpdateSet, |b| b.bind_expr(val))
                    .ok()?;
                self.b.coerce_assign(e, &t.columns[c].ty, &rt.name).ok()?
            };
            self.q().perms[perm].updated.insert(c as i32 + 1 + 7);
            tl.push(Tle {
                expr: e,
                name: Some(rt.name.clone()),
                sortref: 0,
                junk: false,
                origin: (0, 0),
                resno: Some(c as i32 + 1),
            });
        }
        let quals = match s.where_clause.as_deref() {
            Some(w) => {
                let e = self
                    .b
                    .in_clause(Clause::Where, |b| {
                        let e = b.bind_expr(w)?;
                        b.coerce_bool(e, "WHERE")
                    })
                    .ok()?;
                self.expr_text(&e)?
            }
            None => "<>".into(),
        };
        let (mut targets, _) = self.target_list(&tl)?;
        // The MULTIEXPR sublinks, as junk entries after the table's columns.
        if !multi.is_empty() {
            self.q().has_sublinks = true;
            let junk: Vec<String> = multi
                .iter()
                .enumerate()
                .map(|(i, q)| {
                    format!(
                        "{{TARGETENTRY :expr {{SUBLINK :subLinkType 5 :subLinkId {} :testexpr <> :operName <> :subselect {q} :location -1}} :resno {} :resname <> :ressortgroupref 0 :resorigtbl 0 :resorigcol 0 :resjunk true}}",
                        i + 1,
                        t.columns.len() + 1 + i
                    )
                })
                .collect();
            targets = match targets.strip_suffix(')') {
                Some(head) if head.starts_with('(') => format!("{head} {})", junk.join(" ")),
                _ => list(&junk),
            };
        }
        let returning = if s.returning_clause.as_ref().is_some_and(|r| !r.exprs.is_empty()) {
            let r = s.returning_clause.as_ref()?;
            if !r.options.is_empty() {
                return None;
            }
            let tl = self.returning(&r.exprs)?;
            Some(self.target_list(&tl)?)
        } else {
            None
        };
        let r = self.dml_query(Dml {
            command: 2,
            overriding: 0,
            fromlist,
            quals,
            targets,
            on_conflict: "<>".into(),
            cols: returning.as_ref().map(|r| r.1.clone()).unwrap_or_default(),
            returning: returning.map(|r| r.0),
        });
        if with {
            self.b.ctes.pop();
        }
        r
    }

    fn delete(&mut self, s: &pb::DeleteStmt) -> Option<QueryOut> {
        let with = s.with_clause.is_some();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
        }
        let rv = s.relation.as_ref()?;
        let t = self.target_table(rv)?;
        self.result_relation(&t, rv, rv.inh, 8)?;
        let mut fromlist = vec!["{RANGETBLREF :rtindex 1}".to_string()];
        fromlist.extend(self.range_table_items(&s.using_clause)?);
        let quals = match s.where_clause.as_deref() {
            Some(w) => {
                let e = self
                    .b
                    .in_clause(Clause::Where, |b| {
                        let e = b.bind_expr(w)?;
                        b.coerce_bool(e, "WHERE")
                    })
                    .ok()?;
                self.expr_text(&e)?
            }
            None => "<>".into(),
        };
        let returning = if s.returning_clause.as_ref().is_some_and(|r| !r.exprs.is_empty()) {
            let r = s.returning_clause.as_ref()?;
            if !r.options.is_empty() {
                return None;
            }
            let tl = self.returning(&r.exprs)?;
            Some(self.target_list(&tl)?)
        } else {
            None
        };
        let r = self.dml_query(Dml {
            command: 4,
            overriding: 0,
            fromlist,
            quals,
            targets: "<>".into(),
            on_conflict: "<>".into(),
            cols: returning.as_ref().map(|r| r.1.clone()).unwrap_or_default(),
            returning: returning.map(|r| r.0),
        });
        if with {
            self.b.ctes.pop();
        }
        r
    }

    fn merge(&mut self, s: &pb::MergeStmt) -> Option<QueryOut> {
        let returning_clause = s.returning_clause.as_ref().filter(|r| !r.exprs.is_empty());
        if returning_clause.is_some_and(|r| !r.options.is_empty()) {
            return None;
        }
        let with = s.with_clause.is_some();
        if let Some(w) = &s.with_clause {
            self.with_clause(w)?;
        }
        let rv = s.relation.as_ref()?;
        let t = self.target_table(rv)?;
        use pb::CmdType as C;
        use pb::MergeMatchKind as M;
        let mut required = 0;
        for w in &s.merge_when_clauses {
            let NodeEnum::MergeWhenClause(w) = w.node.as_ref()? else {
                return None;
            };
            required |= match C::try_from(w.command_type).ok()? {
                C::CmdInsert => 1,
                C::CmdUpdate => 4,
                C::CmdDelete => 8,
                _ => 0,
            };
        }
        // RETURNING reads the target.
        if returning_clause.is_some() {
            required |= 2;
        }
        let (perm, _) = self.result_relation(&t, rv, rv.inh, required)?;
        let target_scope = std::mem::take(&mut self.b.level().scope);
        let (node, source_scope, _, _) = self.range_item(s.source_relation.as_deref()?)?;
        let both = Scope {
            rels: target_scope
                .rels
                .iter()
                .cloned()
                .chain(source_scope.rels.iter().cloned())
                .collect(),
        };
        self.b.level().scope = both.clone();
        let on = s.join_condition.as_deref()?;
        let cond = self
            .b
            .in_clause(Clause::JoinOn, |b| {
                let e = b.bind_expr(on)?;
                b.coerce_bool(e, "JOIN/ON")
            })
            .ok()?;
        let cond = self.expr_text(&cond)?;
        let mut actions = Vec::new();
        for w in &s.merge_when_clauses {
            let NodeEnum::MergeWhenClause(w) = w.node.as_ref()? else {
                return None;
            };
            let kind = M::try_from(w.match_kind).ok()?;
            self.b.level().scope = match kind {
                M::MergeWhenMatched => both.clone(),
                M::MergeWhenNotMatchedBySource => target_scope.clone(),
                _ => source_scope.clone(),
            };
            let qual = match w.condition.as_deref() {
                Some(c) => {
                    let e = self
                        .b
                        .in_clause(Clause::MergeWhen, |b| {
                            let e = b.bind_expr(c)?;
                            b.coerce_bool(e, "WHEN")
                        })
                        .ok()?;
                    self.expr_text(&e)?
                }
                None => "<>".into(),
            };
            let cmd = C::try_from(w.command_type).ok()?;
            let mut tl = Vec::new();
            match cmd {
                C::CmdUpdate => {
                    for n in &w.target_list {
                        let NodeEnum::ResTarget(rt) = n.node.as_ref()? else {
                            return None;
                        };
                        if !rt.indirection.is_empty() {
                            return None;
                        }
                        let c = t.columns.iter().position(|c| !c.dropped && c.name == rt.name)?;
                        let val = rt.val.as_deref()?;
                        if matches!(val.node.as_ref(), Some(NodeEnum::SetToDefault(_) | NodeEnum::MultiAssignRef(_))) {
                            return None;
                        }
                        let e = self.b.in_clause(Clause::UpdateSet, |b| b.bind_expr(val)).ok()?;
                        let e = self.b.coerce_assign(e, &t.columns[c].ty, &rt.name).ok()?;
                        self.q().perms[perm].updated.insert(c as i32 + 1 + 7);
                        tl.push(Tle {
                            expr: e,
                            name: Some(rt.name.clone()),
                            sortref: 0,
                            junk: false,
                            origin: (0, 0),
                            resno: Some(c as i32 + 1),
                        });
                    }
                }
                C::CmdInsert => {
                    let mut cols: Vec<usize> = Vec::new();
                    for c in &w.target_list {
                        let NodeEnum::ResTarget(rt) = c.node.as_ref()? else {
                            return None;
                        };
                        if !rt.indirection.is_empty() {
                            return None;
                        }
                        cols.push(t.columns.iter().position(|c| !c.dropped && c.name == rt.name)?);
                    }
                    if cols.is_empty() {
                        cols = (0..t.columns.len()).filter(|&i| !t.columns[i].dropped).collect();
                    }
                    if w.values.len() > cols.len() {
                        return None;
                    }
                    for (k, v) in w.values.iter().enumerate() {
                        if matches!(v.node.as_ref(), Some(NodeEnum::SetToDefault(_))) {
                            return None;
                        }
                        let c = cols[k];
                        let e = self.b.in_clause(Clause::ValuesSingle, |b| b.bind_expr(v)).ok()?;
                        let e = self.b.coerce_assign(e, &t.columns[c].ty, &t.columns[c].name).ok()?;
                        self.q().perms[perm].inserted.insert(c as i32 + 1 + 7);
                        tl.push(Tle {
                            expr: e,
                            name: Some(t.columns[c].name.clone()),
                            sortref: 0,
                            junk: false,
                            origin: (0, 0),
                            resno: Some(c as i32 + 1),
                        });
                    }
                }
                C::CmdDelete | C::CmdNothing => {}
                _ => return None,
            }
            let (targets, _) = self.target_list(&tl)?;
            actions.push(format!(
                "{{MERGEACTION :matchKind {} :commandType {} :override {} :qual {qual} :targetList {targets} :updateColnos <>}}",
                w.match_kind - 1,
                w.command_type - 1,
                (w.r#override - 1).max(0)
            ));
        }
        // RETURNING sees both relations, and merge_action().
        let returning = match returning_clause {
            Some(r) => {
                self.b.level().scope = both.clone();
                let col = {
                    let q = self.q();
                    let c = q.next_col;
                    q.next_col += 1;
                    q.merge_action = Some(c);
                    c
                };
                let saved = self.b.merge_action.replace((self.lvl(), col));
                let tl = self.returning(&r.exprs);
                self.b.merge_action = saved;
                Some(self.target_list(&tl?)?)
            }
            None => None,
        };
        let cte_list = self.cte_list();
        let q = self.q();
        let text = format!(
            "{{QUERY :commandType 5 :querySource 0 :canSetTag true :utilityStmt <> :resultRelation 1 :hasAggs false :hasWindowFuncs false :hasTargetSRFs false :hasSubLinks {} :hasDistinctOn false :hasRecursive false :hasModifyingCTE false :hasForUpdate false :hasRowSecurity false :hasGroupRTE false :isReturn false :cteList {cte_list} :rtable {} :rteperminfos {} :jointree {{FROMEXPR :fromlist ({node}) :quals <>}} :mergeActionList {} :mergeTargetRelation 1 :mergeJoinCondition {cond} :targetList <> :override 0 :onConflict <> :returningOldAlias {} :returningNewAlias {} :returningList {} :groupClause <> :groupDistinct false :groupingSets <> :havingQual <> :windowClause <> :distinctClause <> :sortClause <> :limitOffset <> :limitCount <> :limitOption 0 :rowMarks <> :setOperations <> :constraintDeps <> :withCheckOptions <> :stmt_location -1 :stmt_len -1}}",
            q.has_sublinks,
            list(&q.rtable),
            perms_text(&q.perms),
            list(&actions),
            if returning.is_some() { "old" } else { "<>" },
            if returning.is_some() { "new" } else { "<>" },
            returning.as_ref().map_or("<>", |r| r.0.as_str()),
        );
        let text = self.finish(text);
        if with {
            self.b.ctes.pop();
        }
        Some(QueryOut {
            text,
            cols: returning.map(|r| r.1).unwrap_or_default(),
        })
    }

    // ── expressions ──

    fn coll(&self, e: &Expr) -> Coll {
        let known = self.colls.last().and_then(|m| m.get(&(e as *const Expr as usize)));
        match known {
            Some(c) => c.clone().unwrap_or(Coll::None),
            None => self.b.collation_of(e).unwrap_or(Coll::None),
        }
    }

    /// The collation the inputs derive (`inputcollid`) and the result's (for a collatable
    /// result type: that one, else the type's default).
    fn collations(&self, args: &[&Expr], ty: &DataType) -> (u32, u32) {
        let mut c = Coll::None;
        for a in args {
            c = c
                .merge(self.coll(a))
                .unwrap_or(Coll::Conflict(String::new(), String::new()));
        }
        let input = coll_oid(&c);
        let result = if ty.collatable() {
            if input != 0 { input } else { type_collation(ty) }
        } else {
            0
        };
        (input, result)
    }

    fn expr_text(&mut self, e: &Expr) -> Option<String> {
        let mut out = String::new();
        let lvl = self.lvl();
        self.expr(&mut out, e, lvl)?;
        Some(out)
    }

    /// An expression's text, not marking the columns it reads (join alias variables).
    fn expr_text_nomark(&mut self, e: &Expr) -> Option<String> {
        let lvl = self.lvl();
        let perms = self.qs.get(&lvl)?.perms.clone();
        let r = self.expr_text(e);
        self.qs.get_mut(&lvl)?.perms = perms;
        r
    }

    fn list_into(&mut self, out: &mut String, items: &[&Expr], lvl: usize) -> Option<()> {
        if items.is_empty() {
            out.push_str("<>");
            return Some(());
        }
        out.push('(');
        for (i, a) in items.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            self.expr(out, a, lvl)?;
        }
        out.push(')');
        Some(())
    }

    fn konst(&mut self, out: &mut String, v: &Value, ty: &DataType) -> Option<()> {
        // (A NULL array: a varlena.)
        let (len, byval) = match crate::types::ondisk::meta(ty) {
            Some(m) => (m.len, m.byval),
            None if matches!(v, Value::Null) && matches!(ty, DataType::Array(_)) => (-1, false),
            None => return None,
        };
        let _ = write!(
            out,
            "{{CONST :consttype {} :consttypmod {} :constcollid {} :constlen {len} :constbyval {byval} :constisnull {} :location -1 :constvalue ",
            ty.oid(),
            ty.typmod(),
            type_collation(ty),
            matches!(v, Value::Null)
        );
        if matches!(v, Value::Null) {
            out.push_str("<>");
        } else {
            let cat = self.b.ctx.catalog();
            let enum_oid = |t: u32, label: &str| {
                cat.enums
                    .get(&t)
                    .and_then(|e| e.labels.iter().position(|l| l == label).map(|i| e.label_oid(i)))
                    .unwrap_or(0)
            };
            datum(out, v, ty, &enum_oid)?;
        }
        out.push('}');
        Some(())
    }

    /// A Var for column position `col` of level `lvl`, read from the level `cur` (the
    /// query being written).
    fn var(&mut self, out: &mut String, col: usize, lvl: usize, cur: usize) -> Option<()> {
        let q = self.qs.get_mut(&lvl)?;
        if q.merge_action == Some(col) {
            out.push_str("{MERGESUPPORTFUNC :msftype 25 :msfcollid 100 :location -1}");
            return Some(());
        }
        let v = q.cols.get(&col)?.clone();
        if let Some(p) = v.perm {
            let perm = q.perms.get_mut(p)?;
            perm.required |= 2;
            perm.selected.insert(v.attno + 7);
        }
        let nulled = q.nulled.get(&v.rte).cloned().unwrap_or_default();
        let (synno, synatt) = v.syn.unwrap_or((v.rte, v.attno));
        let _ = write!(
            out,
            "{{VAR :varno {0} :varattno {1} :vartype {2} :vartypmod {3} :varcollid {4} :varnullingrels {5} :varlevelsup {6} :varreturningtype 0 :varnosyn {synno} :varattnosyn {synatt} :location -1}}",
            v.rte,
            v.attno,
            v.vartype,
            v.typmod,
            v.coll,
            bitmap(&nulled.iter().map(|&x| x as i32).collect()),
            cur.checked_sub(lvl)?,
        );
        Some(())
    }

    /// `e`, bound at binder level `lvl`, as the query at the current level reads it.
    fn expr(&mut self, out: &mut String, e: &Expr, lvl: usize) -> Option<()> {
        // An expression not inside the one being written: its nodes' collations, derived
        // once (`coll`). (While `e` is borrowed its nodes' addresses are theirs alone.)
        let known = self.colls.last().is_some_and(|m| m.contains_key(&(e as *const Expr as usize)));
        if known {
            return self.expr_node(out, e, lvl);
        }
        let colls = self.b.collations_below(e);
        self.colls.push(colls);
        let r = self.expr_node(out, e, lvl);
        self.colls.pop();
        r
    }

    fn expr_node(&mut self, out: &mut String, e: &Expr, lvl: usize) -> Option<()> {
        let cur = self.lvl();
        // After grouping, a GROUP BY expression is the RTE_GROUP's column.
        if self.subst == Some(lvl)
            && let Some(q) = self.qs.get(&lvl)
            && let Some(i) = q.group.iter().position(|g| g.same_as(e))
        {
            let ty = e.ty();
            let rte = q.group_rte;
            let nulled = if q.group_nullable.get(i).copied().unwrap_or(false) {
                format!("(b {rte})")
            } else {
                "(b)".into()
            };
            let (_, coll) = self.collations(&[e], &ty);
            let _ = write!(
                out,
                "{{VAR :varno {rte} :varattno {0} :vartype {1} :vartypmod {2} :varcollid {coll} :varnullingrels {nulled} :varlevelsup {3} :varreturningtype 0 :varnosyn {rte} :varattnosyn {0} :location -1}}",
                i + 1,
                ty.oid(),
                expr_typmod(e),
                cur - lvl
            );
            return Some(());
        }
        match e {
            Expr::Const(v, ty) => self.konst(out, v, ty)?,
            Expr::Grouping(pos) => {
                let q = self.qs.get(&lvl)?;
                let args: Vec<Expr> = pos.iter().map(|&p| q.grouping_args.get(p).cloned()).collect::<Option<_>>()?;
                let refs: Vec<usize> = pos.iter().map(|&p| q.grouping_refs.get(p).copied()).collect::<Option<_>>()?;
                out.push_str("{GROUPINGFUNC :args (");
                // Its arguments as written, not the grouping step's columns.
                let saved = self.subst.take();
                for (k, a) in args.iter().enumerate() {
                    if k > 0 {
                        out.push(' ');
                    }
                    let r = self.expr(out, a, lvl);
                    if r.is_none() {
                        self.subst = saved;
                        return None;
                    }
                }
                self.subst = saved;
                let _ = write!(
                    out,
                    ") :refs {} :cols <> :agglevelsup {} :location -1}}",
                    int_list('i', &refs),
                    cur - lvl
                );
            }
            Expr::Param(i, ty) => {
                // A MULTIEXPR sublink's column (sublink id << 16 | column).
                let kind = if i + 1 >= 1 << 16 { 3 } else { 0 };
                let _ = write!(
                    out,
                    "{{PARAM :paramkind {kind} :paramid {} :paramtype {} :paramtypmod -1 :paramcollid {} :location -1}}",
                    i + 1,
                    ty.oid(),
                    type_collation(ty)
                );
            }
            // An outer reference: the level's param, an expression one level out.
            Expr::Outer(1, i, _) => {
                let p = self.b.levels.get(lvl)?.params.get(*i)?.clone();
                match p {
                    Expr::Param(..) => self.expr(out, &p, lvl)?,
                    p => {
                        let saved = self.subst.take();
                        let r = self.expr(out, &p, lvl.checked_sub(1)?);
                        self.subst = saved;
                        r?
                    }
                }
            }
            Expr::Col(i, _) => self.var(out, *i, lvl, cur)?,
            Expr::Call(def, args, ty) => {
                if def.variadic || def.is_srf() {
                    return None;
                }
                let refs: Vec<&Expr> = args.iter().collect();
                let (input, result) = self.collations(&refs, ty);
                let symbol = !def.name.is_empty()
                    && def
                        .name
                        .chars()
                        .all(|c| "+-*/<>=~!@#%^&|`?".contains(c));
                if symbol {
                    // A negated numeric literal is a constant.
                    if def.name == "-"
                        && let [Expr::Const(v, cty)] = args.as_slice()
                        && matches!(
                            v,
                            Value::Int2(_) | Value::Int4(_) | Value::Int8(_) | Value::Numeric(_)
                        )
                        && cty == ty
                    {
                        let neg = crate::types::cast::cast_value(
                            Value::Text(format!("-{}", crate::types::text::format_value(v, cty, self.b.ctx.fmt())).into()),
                            &DataType::Unknown,
                            cty,
                            self.b.ctx.fmt(),
                            false,
                        )
                        .ok()?;
                        return self.konst(out, &neg, cty);
                    }
                    let (l, r) = match args.as_slice() {
                        [a, b] => (a.ty(), b.ty()),
                        [a] => (DataType::Void, a.ty()),
                        _ => return None,
                    };
                    let l_oid = if args.len() == 1 { 0 } else { l.oid() };
                    let o = oper_typed(def.name, l_oid, r.oid(), l.is_array(), r.is_array())?;
                    let _ = write!(
                        out,
                        "{{OPEXPR :opno {} :opfuncid {} :opresulttype {} :opretset false :opcollid {result} :inputcollid {input} :args ",
                        o.oid,
                        o.code,
                        ty.oid()
                    );
                } else {
                    if def.oid == 0 || !PROCS.iter().any(|p| p.0 == def.oid) && def.oid < 16384 {
                        return None;
                    }
                    let _ = write!(
                        out,
                        "{{FUNCEXPR :funcid {} :funcresulttype {} :funcretset false :funcvariadic false :funcformat 0 :funccollid {result} :inputcollid {input} :args ",
                        def.oid,
                        ty.oid()
                    );
                }
                self.list_into(out, &refs, lvl)?;
                out.push_str(" :location -1}");
            }
            Expr::Cast(inner, from, to, explicit)
                if to.typmod() != -1 && !matches!(to, DataType::Interval(_)) =>
            {
                // A length coercion over the cast to the unmodified type
                // (coerce_type_typmod), which shows as implicit.
                let base = to.without_typmod();
                let (func, nargs) = match to {
                    DataType::Varchar(_) => (669, 3),
                    DataType::Bpchar(_) => (668, 3),
                    DataType::Bit(_) => (1685, 3),
                    DataType::Varbit(_) => (1687, 3),
                    DataType::Numeric(_) => (1703, 2),
                    DataType::Timestamp(_) => (1961, 2),
                    DataType::Timestamptz(_) => (1967, 2),
                    DataType::Time(_) => (1968, 2),
                    DataType::Timetz(_) => (1969, 2),
                    _ => return None,
                };
                let (_, result) = self.collations(&[&**inner], to);
                let _ = write!(
                    out,
                    "{{FUNCEXPR :funcid {func} :funcresulttype {} :funcretset false :funcvariadic false :funcformat {} :funccollid {result} :inputcollid {result} :args (",
                    to.oid(),
                    if *explicit { 1 } else { 2 }
                );
                if from.same_base(to) && !matches!(from, DataType::Unknown) {
                    self.expr(out, inner, lvl)?;
                } else {
                    let c = Expr::Cast(inner.clone(), from.clone(), base, false);
                    self.expr(out, &c, lvl)?;
                }
                out.push(' ');
                self.konst(out, &Value::Int4(to.typmod()), &DataType::Int4)?;
                if nargs == 3 {
                    out.push(' ');
                    self.konst(out, &Value::Bool(*explicit), &DataType::Bool)?;
                }
                out.push_str(") :location -1}");
            }
            // Functions written in SQL's own syntax: the value functions (CURRENT_DATE, …)
            // are their own node; the others show as such calls.
            Expr::SqlSyntax(inner) => {
                let Expr::Call(def, args, ty) = &**inner else {
                    return self.expr(out, inner, lvl);
                };
                let svf = match def.name {
                    "current_date" => Some(0),
                    "current_time" => Some(if args.is_empty() { 1 } else { 2 }),
                    "current_timestamp" => Some(if args.is_empty() { 3 } else { 4 }),
                    "localtime" => Some(if args.is_empty() { 5 } else { 6 }),
                    "localtimestamp" => Some(if args.is_empty() { 7 } else { 8 }),
                    "current_role" => Some(9),
                    "current_user" => Some(10),
                    "user" => Some(11),
                    "session_user" => Some(12),
                    "current_catalog" => Some(13),
                    "current_schema" => Some(14),
                    _ => None,
                };
                if let Some(op) = svf {
                    let _ = write!(
                        out,
                        "{{SQLVALUEFUNCTION :op {op} :type {} :typmod {} :location -1}}",
                        ty.oid(),
                        ty.typmod()
                    );
                    return Some(());
                }
                let mut s = String::new();
                self.expr(&mut s, inner, lvl)?;
                if !s.starts_with("{FUNCEXPR ") {
                    return None;
                }
                out.push_str(&s.replacen(":funcformat 0 ", ":funcformat 3 ", 1));
            }
            Expr::Relabel(inner, to) => {
                out.push_str("{RELABELTYPE :arg ");
                self.expr(out, inner, lvl)?;
                let _ = write!(
                    out,
                    " :resulttype {} :resulttypmod -1 :resultcollid {} :relabelformat 2 :location -1}}",
                    to.oid(),
                    type_collation(to)
                );
            }
            // `ROW(...)::t`, kept as written for deparsing: a RowExpr of the named type.
            Expr::Cast(inner, _, to @ DataType::Row(r), true)
                if r.oid != 0 && matches!(**inner, Expr::Row(..)) =>
            {
                let Expr::Row(items, _) = &**inner else {
                    return None;
                };
                self.expr(out, &Expr::Row(items.clone(), to.clone()), lvl)?;
            }
            Expr::Cast(inner, from, to, explicit) => {
                // An unknown-typed literal becomes a constant of the type.
                if matches!(from, DataType::Unknown) {
                    let Expr::Const(v, _) = &**inner else {
                        return None;
                    };
                    let v = crate::types::cast::cast_value(
                        v.clone(),
                        from,
                        to,
                        self.b.ctx.fmt(),
                        *explicit,
                    )
                    .ok()?;
                    return self.konst(out, &v, to);
                }
                if to.typmod() != -1 {
                    return None;
                }
                let format = if *explicit { 1 } else { 2 };
                let (_, result) = self.collations(&[&**inner], to);
                let result = if to.collatable() {
                    // A cast's result takes the type's collation, unless its input's.
                    if from.collatable() { result } else { type_collation(to) }
                } else {
                    0
                };
                match casts().get(&(from.oid(), to.oid())).copied() {
                    _ if from.oid() == to.oid() => return self.expr(out, inner, lvl),
                    Some((func, 'f')) => {
                        let (input, _) = self.collations(&[&**inner], to);
                        let _ = write!(
                            out,
                            "{{FUNCEXPR :funcid {func} :funcresulttype {} :funcretset false :funcvariadic false :funcformat {format} :funccollid {result} :inputcollid {input} :args (",
                            to.oid()
                        );
                        self.expr(out, inner, lvl)?;
                        out.push_str(") :location -1}");
                    }
                    Some((_, 'b')) => {
                        out.push_str("{RELABELTYPE :arg ");
                        self.expr(out, inner, lvl)?;
                        let _ = write!(
                            out,
                            " :resulttype {} :resulttypmod -1 :resultcollid {result} :relabelformat {format} :location -1}}",
                            to.oid()
                        );
                    }
                    Some((_, 'i')) | None
                        if from.is_string() || to.is_string() || casts().contains_key(&(from.oid(), to.oid())) =>
                    {
                        out.push_str("{COERCEVIAIO :arg ");
                        self.expr(out, inner, lvl)?;
                        let _ = write!(
                            out,
                            " :resulttype {} :resultcollid {result} :coerceformat {format} :location -1}}",
                            to.oid()
                        );
                    }
                    _ => return None,
                }
            }
            Expr::And(items) | Expr::Or(items) => {
                let op = if matches!(e, Expr::And(_)) { "and" } else { "or" };
                let _ = write!(out, "{{BOOLEXPR :boolop {op} :args ");
                let refs: Vec<&Expr> = items.iter().collect();
                self.list_into(out, &refs, lvl)?;
                out.push_str(" :location -1}");
            }
            Expr::Not(a) => {
                out.push_str("{BOOLEXPR :boolop not :args (");
                self.expr(out, a, lvl)?;
                out.push_str(") :location -1}");
            }
            Expr::IsNull(a, not) => {
                if matches!(a.ty(), DataType::Row(_) | DataType::Record) {
                    return None;
                }
                out.push_str("{NULLTEST :arg ");
                self.expr(out, a, lvl)?;
                let _ = write!(
                    out,
                    " :nulltesttype {} :argisrow false :location -1}}",
                    u8::from(*not)
                );
            }
            Expr::BoolTest(a, t) => {
                out.push_str("{BOOLEANTEST :arg ");
                self.expr(out, a, lvl)?;
                let n = match t {
                    BoolTest::IsTrue => 0,
                    BoolTest::IsNotTrue => 1,
                    BoolTest::IsFalse => 2,
                    BoolTest::IsNotFalse => 3,
                    BoolTest::IsUnknown => 4,
                    BoolTest::IsNotUnknown => 5,
                };
                let _ = write!(out, " :booltesttype {n} :location -1}}");
            }
            Expr::Case {
                operand: None,
                whens,
                else_,
                ty,
            } => {
                let mut results: Vec<&Expr> = whens.iter().map(|(_, r)| r).collect();
                if let Some(e) = else_ {
                    results.push(e);
                }
                let (_, coll) = self.collations(&results, ty);
                let _ = write!(
                    out,
                    "{{CASEEXPR :casetype {} :casecollid {coll} :arg <> :args (",
                    ty.oid()
                );
                for (i, (w, r)) in whens.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str("{CASEWHEN :expr ");
                    self.expr(out, w, lvl)?;
                    out.push_str(" :result ");
                    self.expr(out, r, lvl)?;
                    out.push_str(" :location -1}");
                }
                out.push_str(") :defresult ");
                match else_ {
                    Some(e) => self.expr(out, e, lvl)?,
                    None => self.konst(out, &Value::Null, ty)?,
                }
                out.push_str(" :location -1}");
            }
            Expr::Coalesce(items, ty) => {
                let refs: Vec<&Expr> = items.iter().collect();
                let (_, coll) = self.collations(&refs, ty);
                let _ = write!(
                    out,
                    "{{COALESCEEXPR :coalescetype {} :coalescecollid {coll} :args ",
                    ty.oid()
                );
                self.list_into(out, &refs, lvl)?;
                out.push_str(" :location -1}");
            }
            Expr::MinMax(items, greatest, ty, _) => {
                let refs: Vec<&Expr> = items.iter().collect();
                let (input, result) = self.collations(&refs, ty);
                let _ = write!(
                    out,
                    "{{MINMAXEXPR :minmaxtype {} :minmaxcollid {result} :inputcollid {input} :op {} :args ",
                    ty.oid(),
                    u8::from(!*greatest)
                );
                self.list_into(out, &refs, lvl)?;
                out.push_str(" :location -1}");
            }
            // OpExprs under other names; IS NOT DISTINCT FROM is NOT over IS DISTINCT FROM.
            Expr::NullIf(a, b, ..) | Expr::IsDistinct(a, b, ..) => {
                let (node, not) = match e {
                    Expr::IsDistinct(_, _, not, _) => ("DISTINCTEXPR", *not),
                    _ => ("NULLIFEXPR", false),
                };
                if not {
                    out.push_str("{BOOLEXPR :boolop not :args (");
                }
                let o = oper("=", a.ty().oid(), b.ty().oid())?;
                let rty = if node == "NULLIFEXPR" { a.ty() } else { DataType::Bool };
                let (input, result) = self.collations(&[&**a, &**b], &rty);
                let _ = write!(
                    out,
                    "{{{node} :opno {} :opfuncid {} :opresulttype {} :opretset false :opcollid {result} :inputcollid {input} :args ",
                    o.oid,
                    o.code,
                    rty.oid()
                );
                self.list_into(out, &[&**a, &**b], lvl)?;
                out.push_str(" :location -1}");
                if not {
                    out.push_str(") :location -1}");
                }
            }
            Expr::Subscript(base, subs, ty) => {
                let bty = base.ty();
                let elem = bty.elem()?.clone();
                let slice = subs.iter().any(|s| s.2);
                let _ = write!(
                    out,
                    "{{SUBSCRIPTINGREF :refcontainertype {} :refelemtype {} :refrestype {} :reftypmod -1 :refcollid {} :refupperindexpr (",
                    bty.oid(),
                    elem.oid(),
                    ty.oid(),
                    type_collation(ty)
                );
                for (n, (lo, hi, is_slice)) in subs.iter().enumerate() {
                    let x = if slice || *is_slice { hi.as_ref() } else { hi.as_ref().or(lo.as_ref()) };
                    if n > 0 {
                        out.push(' ');
                    }
                    match x {
                        Some(x) => self.expr(out, x, lvl)?,
                        None => out.push_str("<>"),
                    }
                }
                out.push_str(") :reflowerindexpr ");
                if slice {
                    out.push('(');
                    for (n, (lo, _, _)) in subs.iter().enumerate() {
                        if n > 0 {
                            out.push(' ');
                        }
                        match lo {
                            Some(x) => self.expr(out, x, lvl)?,
                            None => out.push_str("<>"),
                        }
                    }
                    out.push(')');
                } else {
                    out.push_str("<>");
                }
                out.push_str(" :refexpr ");
                self.expr(out, base, lvl)?;
                out.push_str(" :refassgnexpr <>}");
            }
            // An assignment to an element or slice.
            Expr::Assign {
                base,
                subs,
                field: None,
                value,
                ty,
            } if !subs.is_empty() => {
                let elem = ty.elem()?.clone();
                let slice = subs.iter().any(|s| s.2);
                let _ = write!(
                    out,
                    "{{SUBSCRIPTINGREF :refcontainertype {0} :refelemtype {1} :refrestype {0} :reftypmod -1 :refcollid {2} :refupperindexpr (",
                    ty.oid(),
                    elem.oid(),
                    type_collation(ty)
                );
                for (n, (lo, hi, is_slice)) in subs.iter().enumerate() {
                    let x = if slice || *is_slice { hi.as_ref() } else { hi.as_ref().or(lo.as_ref()) };
                    if n > 0 {
                        out.push(' ');
                    }
                    match x {
                        Some(x) => self.expr(out, x, lvl)?,
                        None => out.push_str("<>"),
                    }
                }
                out.push_str(") :reflowerindexpr ");
                if slice {
                    out.push('(');
                    for (n, (lo, _, _)) in subs.iter().enumerate() {
                        if n > 0 {
                            out.push(' ');
                        }
                        match lo {
                            Some(x) => self.expr(out, x, lvl)?,
                            None => out.push_str("<>"),
                        }
                    }
                    out.push(')');
                } else {
                    out.push_str("<>");
                }
                out.push_str(" :refexpr ");
                self.expr(out, base, lvl)?;
                out.push_str(" :refassgnexpr ");
                self.expr(out, value, lvl)?;
                out.push('}');
            }
            // DEFAULT for a column (see `set_to_default`).
            Expr::Collate(a, name, -2) if matches!(**a, Expr::Default(_)) => {
                let Expr::Default(ty) = &**a else {
                    return None;
                };
                let _ = write!(
                    out,
                    "{{SETTODEFAULT :typeId {} :typeMod {} :collation {} :location -1}}",
                    ty.oid(),
                    ty.typmod(),
                    name.parse::<u32>().ok()?
                );
            }
            Expr::Collate(a, name, _) => {
                out.push_str("{COLLATEEXPR :arg ");
                self.expr(out, a, lvl)?;
                let _ = write!(out, " :collOid {} :location -1}}", collation_oid(name)?);
            }
            Expr::Agg(i, ty) => self.aggref(out, *i, ty, lvl)?,
            Expr::Window(i, ty) => self.window_func(out, *i, ty, lvl)?,
            Expr::Srf(i, ty) => {
                let call = self.b.levels.get(lvl)?.st.srfs.get(*i)?.clone();
                if call.body.is_some() || call.ncols != 1 || call.func.oid == 0 {
                    return None;
                }
                let refs: Vec<&Expr> = call.args.iter().collect();
                let (input, result) = self.collations(&refs, ty);
                let _ = write!(
                    out,
                    "{{FUNCEXPR :funcid {} :funcresulttype {} :funcretset true :funcvariadic false :funcformat 0 :funccollid {result} :inputcollid {input} :args ",
                    call.func.oid,
                    ty.oid()
                );
                self.list_into(out, &refs, lvl)?;
                out.push_str(" :location -1}");
            }
            Expr::Subquery(s) => self.sublink(out, s, lvl)?,
            Expr::Array(items, ty) => {
                let elem = ty.elem()?.clone();
                let refs: Vec<&Expr> = items.iter().collect();
                let (_, coll) = self.collations(&refs, ty);
                let multi = items.iter().any(|i| matches!(i, Expr::Array(..)));
                let _ = write!(
                    out,
                    "{{ARRAYEXPR :array_typeid {} :array_collid {coll} :element_typeid {} :elements ",
                    ty.oid(),
                    elem.oid()
                );
                if refs.is_empty() {
                    out.push_str("<>");
                } else {
                    self.list_into(out, &refs, lvl)?;
                }
                let _ = write!(out, " :multidims {multi} :list_start -1 :list_end -1 :location -1}}");
            }
            Expr::ArrayOp {
                left,
                right,
                op,
                all,
            } => {
                let (lt, rt) = (left.ty(), right.ty());
                let elem = rt.elem()?.clone();
                let o = oper_typed(op.name, lt.oid(), elem.oid(), lt.is_array(), elem.is_array())?;
                let (input, _) = self.collations(&[&**left, &**right], &DataType::Bool);
                let _ = write!(
                    out,
                    "{{SCALARARRAYOPEXPR :opno {} :opfuncid {} :hashfuncid 0 :negfuncid 0 :useOr {} :inputcollid {input} :args ",
                    o.oid,
                    o.code,
                    !all
                );
                self.list_into(out, &[&**left, &**right], lvl)?;
                out.push_str(" :location -1}");
            }
            Expr::Case {
                operand: Some(arg),
                whens,
                else_,
                ty,
            } => {
                let mut results: Vec<&Expr> = whens.iter().map(|(_, r)| r).collect();
                if let Some(e) = else_ {
                    results.push(e);
                }
                let (_, coll) = self.collations(&results, ty);
                let _ = write!(out, "{{CASEEXPR :casetype {} :casecollid {coll} :arg ", ty.oid());
                self.expr(out, arg, lvl)?;
                out.push_str(" :args (");
                for (i, (w, r)) in whens.iter().enumerate() {
                    if i > 0 {
                        out.push(' ');
                    }
                    out.push_str("{CASEWHEN :expr ");
                    self.expr(out, w, lvl)?;
                    out.push_str(" :result ");
                    self.expr(out, r, lvl)?;
                    out.push_str(" :location -1}");
                }
                out.push_str(") :defresult ");
                match else_ {
                    Some(e) => self.expr(out, e, lvl)?,
                    None => self.konst(out, &Value::Null, ty)?,
                }
                out.push_str(" :location -1}");
            }
            Expr::CaseTest(ty) => {
                let _ = write!(
                    out,
                    "{{CASETESTEXPR :typeId {} :typeMod {} :collation {}}}",
                    ty.oid(),
                    ty.typmod(),
                    type_collation(ty)
                );
            }
            Expr::Field(base, i, ty) => {
                out.push_str("{FIELDSELECT :arg ");
                self.expr(out, base, lvl)?;
                let (_, coll) = self.collations(&[e], ty);
                let _ = write!(
                    out,
                    " :fieldnum {} :resulttype {} :resulttypmod {} :resultcollid {coll}}}",
                    i + 1,
                    ty.oid(),
                    ty.typmod()
                );
            }
            Expr::Row(items, ty) => {
                let refs: Vec<&Expr> = items.iter().collect();
                // A ROW() made a named composite by a cast (`coerce_record_to_complex`)
                // keeps no column names.
                let (oid, names) = match ty {
                    DataType::Row(r) if r.oid != 0 => (r.oid, Vec::new()),
                    _ => (2249, (1..=items.len()).map(|i| format!("f{i}")).collect()),
                };
                out.push_str("{ROWEXPR :args ");
                self.list_into(out, &refs, lvl)?;
                let _ = write!(
                    out,
                    " :row_typeid {oid} :row_format {} :colnames {} :location -1}}",
                    if oid == 2249 { 2 } else { 1 },
                    names_list(&names)
                );
            }
            Expr::CoerceToDomain(arg, d) if d.tag => self.expr(out, arg, lvl)?,
            Expr::CoerceToDomain(arg, d) if !d.array => {
                out.push_str("{COERCETODOMAIN :arg ");
                self.expr(out, arg, lvl)?;
                let ty = arg.ty();
                let (_, coll) = self.collations(&[arg], &ty);
                let _ = write!(
                    out,
                    " :resulttype {} :resulttypmod {} :resultcollid {coll} :coercionformat 1 :location -1}}",
                    d.oid,
                    ty.typmod()
                );
            }
            _ => return None,
        }
        Some(())
    }

    fn aggref(&mut self, out: &mut String, i: usize, ty: &DataType, lvl: usize) -> Option<()> {
        let call = self.b.levels.get(lvl)?.st.aggs.get(i)?.clone();
        if !call.direct.is_empty() || call.user.is_some() || call.json_ctor {
            return None;
        }
        let name = call.def.sig.name;
        let args = call.args.clone();
        let arg_types: Vec<u32> = call.arg_types.iter().map(DataType::oid).collect();
        let star = args.is_empty();
        let oid = proc_oid(name, &arg_types)?;
        let refs: Vec<&Expr> = args.iter().collect();
        let (input, result) = self.collations(&refs, ty);
        let _ = write!(
            out,
            "{{AGGREF :aggfnoid {oid} :aggtype {} :aggcollid {result} :inputcollid {input} :aggtranstype 0 :aggargtypes {} :aggdirectargs <> :args ",
            ty.oid(),
            int_list('o', &arg_types)
        );
        // The arguments as a target list, with the ORDER BY / DISTINCT keys.
        let mut atl: Vec<(Expr, bool, usize)> = args.iter().map(|a| (a.clone(), false, 0)).collect();
        let next_ref = |atl: &[(Expr, bool, usize)]| atl.iter().map(|t| t.2).max().unwrap_or(0) + 1;
        let mut order = Vec::new();
        for k in &call.order_by {
            let at = match atl.iter().position(|t| t.0.same_as(&k.expr)) {
                Some(i) => i,
                None => {
                    atl.push((k.expr.clone(), true, 0));
                    atl.len() - 1
                }
            };
            if atl[at].2 == 0 {
                atl[at].2 = next_ref(&atl);
            }
            let (eq, op, hash) = sort_ops(&k.expr.ty(), k.desc)?;
            order.push((
                atl[at].2,
                format!(
                    "{{SORTGROUPCLAUSE :tleSortGroupRef {} :eqop {eq} :sortop {op} :reverse_sort {} :nulls_first {} :hashable {hash}}}",
                    atl[at].2, k.desc, k.nulls_first
                ),
            ));
        }
        let mut distinct = Vec::new();
        if call.distinct {
            distinct = order.clone();
            for at in 0..atl.len() {
                if atl[at].1 || distinct.iter().any(|d| d.0 == atl[at].2 && atl[at].2 != 0) {
                    continue;
                }
                if atl[at].2 == 0 {
                    atl[at].2 = next_ref(&atl);
                }
                let (eq, op, hash) = sort_ops(&atl[at].0.ty(), false)?;
                distinct.push((
                    atl[at].2,
                    format!(
                        "{{SORTGROUPCLAUSE :tleSortGroupRef {} :eqop {eq} :sortop {op} :reverse_sort false :nulls_first false :hashable {hash}}}",
                        atl[at].2
                    ),
                ));
            }
        }
        // Its arguments are read before grouping.
        let saved = self.subst.take();
        let r = (|| {
            if star {
                out.push_str("<>");
            } else {
                out.push('(');
                for (n, (a, junk, sref)) in atl.iter().enumerate() {
                    if n > 0 {
                        out.push(' ');
                    }
                    out.push_str("{TARGETENTRY :expr ");
                    self.expr(out, a, lvl)?;
                    let _ = write!(
                        out,
                        " :resno {} :resname <> :ressortgroupref {sref} :resorigtbl 0 :resorigcol 0 :resjunk {junk}}}",
                        n + 1
                    );
                }
                out.push(')');
            }
            let texts = |v: &[(usize, String)]| list(&v.iter().map(|x| x.1.clone()).collect::<Vec<_>>());
            let _ = write!(
                out,
                " :aggorder {} :aggdistinct {} :aggfilter ",
                texts(&order),
                texts(&distinct)
            );
            match &call.filter {
                Some(f) => self.expr(out, f, lvl)?,
                None => out.push_str("<>"),
            }
            Some(())
        })();
        self.subst = saved;
        r?;
        let _ = write!(
            out,
            " :aggstar {star} :aggvariadic false :aggkind n :aggpresorted false :agglevelsup {} :aggsplit 0 :aggno -1 :aggtransno -1 :location -1}}",
            self.lvl() - lvl
        );
        Some(())
    }

    fn window_func(&mut self, out: &mut String, i: usize, ty: &DataType, lvl: usize) -> Option<()> {
        let call = self.b.levels.get(lvl)?.st.windows.get(i)?.clone();
        let winref = *self.qs.get(&lvl)?.winrefs.get(i)?;
        let arg_types: Vec<u32> = call.args.iter().map(|a| a.ty().oid()).collect();
        let (oid, star, agg) = match &call.func {
            WinFunc::Builtin(k) => {
                let name = match k {
                    WinKind::RowNumber => "row_number",
                    WinKind::Rank => "rank",
                    WinKind::DenseRank => "dense_rank",
                    WinKind::PercentRank => "percent_rank",
                    WinKind::CumeDist => "cume_dist",
                    WinKind::Ntile => "ntile",
                    WinKind::Lag => "lag",
                    WinKind::Lead => "lead",
                    WinKind::FirstValue => "first_value",
                    WinKind::LastValue => "last_value",
                    WinKind::NthValue => "nth_value",
                };
                // The polymorphic ones are declared over anyelement / anycompatible.
                let oid = proc_oid(name, &arg_types).or_else(|| {
                    PROCS
                        .iter()
                        .filter(|p| p.1 == name && p.2.len() == arg_types.len())
                        .find(|p| {
                            p.2.iter().zip(&arg_types).all(|(w, h)| {
                                w == h || matches!(*w, 2283 | 5077)
                            })
                        })
                        .map(|p| p.0)
                })?;
                (oid, false, false)
            }
            WinFunc::Agg(a) => {
                if a.distinct || !a.order_by.is_empty() || a.user.is_some() {
                    return None;
                }
                let types: Vec<u32> = a.arg_types.iter().map(DataType::oid).collect();
                (proc_oid(a.def.sig.name, &types)?, call.args.is_empty(), true)
            }
        };
        if call.ignore_nulls {
            return None;
        }
        let refs: Vec<&Expr> = call.args.iter().collect();
        let (input, result) = self.collations(&refs, ty);
        let _ = write!(
            out,
            "{{WINDOWFUNC :winfnoid {oid} :wintype {} :wincollid {result} :inputcollid {input} :args ",
            ty.oid()
        );
        if refs.is_empty() {
            out.push_str("<>");
        } else {
            self.list_into(out, &refs, lvl)?;
        }
        out.push_str(" :aggfilter ");
        match &call.func {
            WinFunc::Agg(a) if a.filter.is_some() => self.expr(out, a.filter.as_ref()?, lvl)?,
            _ => out.push_str("<>"),
        }
        let _ = write!(
            out,
            " :runCondition <> :winref {winref} :winstar {star} :winagg {agg} :location -1}}"
        );
        Some(())
    }

    fn sublink(&mut self, out: &mut String, s: &crate::sql::expr::Subquery, lvl: usize) -> Option<()> {
        if s.func.is_some() || lvl != self.lvl() {
            return None;
        }
        let node = self.b.sublink_nodes.get(&s.id)?.clone();
        let NodeEnum::SelectStmt(sel) = node.node.as_ref()? else {
            return None;
        };
        self.q().has_sublinks = true;
        // (Analyzed again here: an aggregate of this level inside it was allowed where it
        // stood, which this level's clause no longer says.)
        let clause = self.b.level().clause;
        if !clause.aggs_ok() {
            self.b.level().clause = Clause::Select;
        }
        let r = self.sublink_inner(out, s, sel, lvl);
        self.b.level().clause = clause;
        r
    }

    fn sublink_inner(
        &mut self,
        out: &mut String,
        s: &crate::sql::expr::Subquery,
        sel: &pb::SelectStmt,
        lvl: usize,
    ) -> Option<()> {
        let (kind, oper) = match s.kind {
            SubqueryKind::Exists => (0, None),
            SubqueryKind::All => (1, Some(())),
            SubqueryKind::Any => (2, Some(())),
            SubqueryKind::RowCompare => (3, Some(())),
            SubqueryKind::Scalar => (4, None),
            SubqueryKind::Array => (6, None),
        };
        let q = self.nested(|t| t.select(sel, false, true))?;
        let (test, opname) = match oper {
            Some(()) => {
                let mut parts = Vec::new();
                let mut opname = None;
                for (k, (l, op)) in s.left.iter().zip(&s.ops).enumerate() {
                    let col = q.cols.get(k)?;
                    let rty = col.ty.clone();
                    let lty = l.ty();
                    let o = oper_typed(op.name, lty.oid(), rty.oid(), lty.is_array(), rty.is_array())?;
                    opname = Some(op.name);
                    let (input, result) = {
                        let lc = self.coll(l);
                        let rc = col
                            .collation
                            .clone()
                            .unwrap_or_else(|| Coll::Implicit(rty.default_collation().unwrap_or_default().into()));
                        let c = lc.merge(if rty.collatable() { rc } else { Coll::None }).ok()?;
                        let input = coll_oid(&c);
                        (input, if DataType::Bool.collatable() { input } else { 0 })
                    };
                    let mut p = format!(
                        "{{OPEXPR :opno {} :opfuncid {} :opresulttype 16 :opretset false :opcollid {result} :inputcollid {input} :args (",
                        o.oid, o.code
                    );
                    self.expr(&mut p, l, lvl)?;
                    let _ = write!(
                        p,
                        " {{PARAM :paramkind 2 :paramid {} :paramtype {} :paramtypmod {} :paramcollid {} :location -1}}) :location -1}}",
                        k + 1,
                        rty.oid(),
                        col.typmod,
                        col.coll
                    );
                    parts.push(p);
                }
                if parts.len() != 1 {
                    return None;
                }
                let mut name = String::new();
                string_node(&mut name, opname?);
                (parts.pop()?, format!("({name})"))
            }
            None => ("<>".into(), "<>".into()),
        };
        let _ = write!(
            out,
            "{{SUBLINK :subLinkType {kind} :subLinkId 0 :testexpr {test} :operName {opname} :subselect {} :location -1}}",
            q.text
        );
        Some(())
    }
}

/// DEFAULT for column `c` (SetToDefault: its type and collation), as an expression this
/// writes.
fn set_to_default(c: &crate::catalog::Column) -> Expr {
    // (The collation's oid rides as the COLLATE's name.)
    let coll = crate::catalog::system::column_collation(c);
    Expr::Collate(Box::new(Expr::Default(c.ty.clone())), coll.to_string(), -2)
}

/// What INSERT / UPDATE / DELETE fill in of their Query.
struct Dml {
    command: u8,
    /// OVERRIDING USER (1) / SYSTEM (2) VALUE.
    overriding: i32,
    fromlist: Vec<String>,
    quals: String,
    targets: String,
    on_conflict: String,
    returning: Option<String>,
    cols: Vec<OutInfo>,
}

trait PushGet {
    fn push_get(&mut self) -> &mut String;
}

impl PushGet for Vec<String> {
    fn push_get(&mut self) -> &mut String {
        self.push(String::new());
        self.last_mut().unwrap()
    }
}

fn perms_text(perms: &[Perm]) -> String {
    let items: Vec<String> = perms
        .iter()
        .map(|p| {
            format!(
                "{{RTEPERMISSIONINFO :relid {} :inh {} :requiredPerms {} :checkAsUser 0 :selectedCols {} :insertedCols {} :updatedCols {}}}",
                p.relid,
                p.inh,
                p.required,
                bitmap(&p.selected),
                bitmap(&p.inserted),
                bitmap(&p.updated)
            )
        })
        .collect();
    list(&items)
}

/// The select list's nodes, one per entry (none past a `*`).
fn target_nodes(list: &[Node]) -> Vec<Option<&Node>> {
    let mut out: Vec<Option<&Node>> = list.iter().map(Some).collect();
    out.extend(std::iter::repeat_n(None, 4096));
    out
}

/// A window definition as `equal()` compares it (without locations).
fn window_key(w: &pb::WindowDef) -> String {
    let mut w = w.clone();
    w.name.clear();
    w.refname.clear();
    w.location = 0;
    let s = serde_json::to_string(&w).unwrap_or_default();
    let mut out = String::with_capacity(s.len());
    let mut rest = s.as_str();
    while let Some(at) = rest.find("\"location\":") {
        out.push_str(&rest[..at]);
        rest = rest[at + 11..].trim_start_matches(|c: char| c == '-' || c.is_ascii_digit());
    }
    out.push_str(rest);
    out
}

/// The name `FigureColname` gives an analyzed GROUP BY expression.
fn group_colname(e: &Expr) -> String {
    match e {
        Expr::Case { .. } => "case",
        Expr::Coalesce(..) => "coalesce",
        Expr::MinMax(_, true, ..) => "greatest",
        Expr::MinMax(_, false, ..) => "least",
        Expr::Row(..) => "row",
        _ => "?column?",
    }
    .to_string()
}

/// An expression's typmod (`exprTypmod`): a column's, else -1.
fn expr_typmod(e: &Expr) -> i32 {
    match e {
        Expr::Col(_, t) | Expr::Const(_, t) => t.typmod(),
        Expr::Cast(_, _, to, _) => to.typmod(),
        _ => -1,
    }
}

/// A system catalog's / view's relkind.
fn sys_relkind(oid: u32) -> Option<char> {
    use crate::catalog::static_rows::{oid_at, rows};
    let r = rows(SysTable::PgClass)
        .iter()
        .find(|r| oid_at(r, 0) == oid)?
        .clone();
    match r.get(17)? {
        Value::Char(c) => Some(*c as char),
        _ => None,
    }
}

// ── TOAST of a stored node tree ──

/// Whether a view's `_RETURN` rule with action `ev_action` is stored out of line in
/// pg_rewrite's TOAST table (heap_toast_insert_or_update), which takes an oid for the value.
pub fn rule_action_toasted(ev_action: &str) -> bool {
    // The tuple: header (24), fixed columns through is_instead (75), ev_qual "<>" as a short
    // varlena (3), then ev_action (int-aligned, 4-byte header).
    const MAX_DATA: usize = 2032 - 24;
    let len = ev_action.len();
    if 24 + 84 + len <= 2032 {
        return false;
    }
    match pglz_compressed_len(ev_action.as_bytes()) {
        // Compressed inline (8-byte header) when that saves more than 2 bytes.
        Some(c) if c + 8 < len - 2 => 80 + c + 8 > MAX_DATA,
        _ => true,
    }
}

/// The length `pglz_compress` (default strategy) produces for `src`, or None when it gives
/// up (too short, not compressible enough).
pub fn pglz_compressed_len(src: &[u8]) -> Option<usize> {
    const MAX_MATCH: usize = 273;
    const HISTORY: usize = 4096;
    let slen = src.len();
    if slen < 32 {
        return None;
    }
    let good_match_init = 128usize;
    let good_drop = 10usize;
    let result_max = slen * 75 / 100;
    let hashsz: usize = match slen {
        0..128 => 512,
        128..256 => 1024,
        256..512 => 2048,
        512..1024 => 4096,
        _ => 8192,
    };
    let mask = hashsz - 1;
    let idx = |pos: usize| -> usize {
        if slen - pos < 4 {
            src[pos] as usize & mask
        } else {
            (((src[pos] as usize) << 6)
                ^ ((src[pos + 1] as usize) << 4)
                ^ ((src[pos + 2] as usize) << 2)
                ^ src[pos + 3] as usize)
                & mask
        }
    };
    // History entries 1..=HISTORY (0: none), linked per hash slot.
    let mut start = vec![0usize; hashsz];
    let mut next = vec![0usize; HISTORY + 1];
    let mut prev: Vec<Option<usize>> = vec![None; HISTORY + 1];
    let mut hindex = vec![0usize; HISTORY + 1];
    let mut hpos = vec![0usize; HISTORY + 1];
    let mut hn = 1usize;
    let mut recycle = false;
    let mut add = |pos: usize,
                   start: &mut Vec<usize>,
                   next: &mut Vec<usize>,
                   prev: &mut Vec<Option<usize>>,
                   hindex: &mut Vec<usize>,
                   hpos: &mut Vec<usize>| {
        let h = idx(pos);
        let me = hn;
        if recycle {
            match prev[me] {
                None => start[hindex[me]] = next[me],
                Some(p) => next[p] = next[me],
            }
            let n = next[me];
            prev[n] = prev[me];
        }
        next[me] = start[h];
        prev[me] = None;
        hindex[me] = h;
        hpos[me] = pos;
        let head = start[h];
        prev[head] = Some(me);
        start[h] = me;
        hn += 1;
        if hn > HISTORY {
            hn = 1;
            recycle = true;
        }
    };
    let mut out_len = 0usize;
    // Control bits pending in the current control byte (a new one every 8 items).
    let mut ctrl_count = 8usize;
    let mut found = false;
    let mut dp = 0usize;
    while dp < slen {
        if out_len >= result_max {
            return None;
        }
        if !found && out_len >= 1024 {
            return None;
        }
        // The longest match in the history.
        let (mut len, mut off) = (0usize, 0usize);
        let mut good = good_match_init;
        let mut e = start[idx(dp)];
        while e != 0 {
            let hp = hpos[e];
            let thisoff = dp - hp;
            if thisoff >= 0x0fff {
                break;
            }
            let mut thislen = 0usize;
            if len >= 16 {
                if src[dp..dp + len] == src[hp..hp + len] {
                    thislen = len;
                    while dp + thislen < slen && src[dp + thislen] == src[hp + thislen] && thislen < MAX_MATCH {
                        thislen += 1;
                    }
                }
            } else {
                while dp + thislen < slen && src[dp + thislen] == src[hp + thislen] && thislen < MAX_MATCH {
                    thislen += 1;
                }
            }
            if thislen > len {
                len = thislen;
                off = thisoff;
            }
            e = next[e];
            if e != 0 {
                if len >= good {
                    break;
                }
                good -= good * good_drop / 100;
            }
        }
        let _ = off;
        if ctrl_count == 8 {
            out_len += 1;
            ctrl_count = 0;
        }
        ctrl_count += 1;
        if len > 2 {
            out_len += if len > 17 { 3 } else { 2 };
            for _ in 0..len {
                add(dp, &mut start, &mut next, &mut prev, &mut hindex, &mut hpos);
                dp += 1;
            }
            found = true;
        } else {
            out_len += 1;
            add(dp, &mut start, &mut next, &mut prev, &mut hindex, &mut hpos);
            dp += 1;
        }
    }
    (out_len < result_max).then_some(out_len)
}

