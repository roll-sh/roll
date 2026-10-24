//! Publications (publicationcmds.c, pg_publication.c): CREATE / ALTER / DROP PUBLICATION
//! with their options, the tables they name (row filters, column lists) and the schemas
//! (FOR TABLES IN SCHEMA). Nothing is replicated; this is the catalog side, whose rows psql's
//! \dRp and \d show, and whose replica identity rules UPDATE and DELETE check.

use super::Ddl;
use crate::catalog::acl::{self, ObjKind};
use crate::catalog::pubsub::{PubRel, PubSchema, Publication};
use crate::catalog::{Relation, Table};
use crate::error::{Error, Result, code};
use crate::sql::binder::Clause;
use crate::sql::binder::ast::{def_arg_str, inner, names, with_pos};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;
use std::sync::Arc;

/// The first oid of objects not made by initdb.
const FIRST_NORMAL: u32 = crate::catalog::FIRST_USER_OID;

/// parse_publication_options' result: each given option.
#[derive(Default)]
struct PubOptions {
    /// publish: (insert, update, delete, truncate).
    publish: Option<(bool, bool, bool, bool)>,
    via_root: Option<bool>,
    gencols: Option<char>,
}

/// defGetBoolean.
fn def_bool(d: &pb::DefElem) -> Result<bool> {
    let Some(arg) = d.arg.as_deref() else {
        return Ok(true);
    };
    let v = match inner(arg) {
        Some(NodeEnum::Integer(i)) => match i.ival {
            0 => return Ok(false),
            1 => return Ok(true),
            _ => String::new(),
        },
        Some(NodeEnum::Boolean(b)) => return Ok(b.boolval),
        _ => def_arg_str(d).unwrap_or_default(),
    };
    match v.to_ascii_lowercase().as_str() {
        "true" | "on" | "t" | "yes" | "y" | "1" => Ok(true),
        "false" | "off" | "f" | "no" | "n" | "0" => Ok(false),
        _ => Err(Error::new(
            code::SYNTAX_ERROR,
            format!("{} requires a Boolean value", d.defname),
        )),
    }
}

/// SplitIdentifierString with ',' (lower-casing unquoted names).
fn split_identifiers(s: &str) -> Option<Vec<String>> {
    let mut out = Vec::new();
    // SplitIdentifierString: an empty (or all-blank) string is an empty list.
    if s.trim().is_empty() {
        return Some(out);
    }
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let mut name = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            loop {
                match chars.next() {
                    None => return None,
                    Some('"') if chars.peek() == Some(&'"') => {
                        chars.next();
                        name.push('"');
                    }
                    Some('"') => break,
                    Some(c) => name.push(c),
                }
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ',' || c.is_whitespace() {
                    break;
                }
                name.push(c.to_ascii_lowercase());
                chars.next();
            }
            if name.is_empty() {
                return None;
            }
        }
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        out.push(name);
        match chars.next() {
            None => return Some(out),
            Some(',') => continue,
            Some(_) => return None,
        }
    }
}

fn parse_options(opts: &[pb::Node]) -> Result<PubOptions> {
    let mut o = PubOptions::default();
    let conflict = |d: &pb::DefElem| {
        with_pos(
            Error::new(code::SYNTAX_ERROR, "conflicting or redundant options"),
            d.location,
        )
    };
    for d in super::def_elems(opts) {
        match d.defname.as_str() {
            "publish" => {
                if o.publish.is_some() {
                    return Err(conflict(d));
                }
                let v = def_arg_str(d).unwrap_or_default();
                let Some(list) = split_identifiers(&v) else {
                    return Err(Error::new(
                        code::SYNTAX_ERROR,
                        "invalid list syntax in parameter \"publish\"",
                    ));
                };
                let mut p = (false, false, false, false);
                for a in list {
                    match a.as_str() {
                        "insert" => p.0 = true,
                        "update" => p.1 = true,
                        "delete" => p.2 = true,
                        "truncate" => p.3 = true,
                        _ => {
                            return Err(Error::new(
                                code::SYNTAX_ERROR,
                                format!(
                                    "unrecognized value for publication option \"publish\": \"{a}\""
                                ),
                            ));
                        }
                    }
                }
                o.publish = Some(p);
            }
            "publish_via_partition_root" => {
                if o.via_root.is_some() {
                    return Err(conflict(d));
                }
                o.via_root = Some(def_bool(d)?);
            }
            "publish_generated_columns" => {
                if o.gencols.is_some() {
                    return Err(conflict(d));
                }
                let v = if d.arg.is_some() {
                    def_arg_str(d).unwrap_or_default()
                } else {
                    String::new()
                };
                o.gencols = Some(match v.to_ascii_lowercase().as_str() {
                    "none" => 'n',
                    "stored" => 's',
                    _ => {
                        return Err(Error::new(
                            code::SYNTAX_ERROR,
                            format!(
                                "invalid value for publication parameter \"{}\": \"{v}\"",
                                d.defname
                            ),
                        )
                        .detail("Valid values are \"none\" and \"stored\"."));
                    }
                });
            }
            other => {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    format!("unrecognized publication parameter: \"{other}\""),
                ));
            }
        }
    }
    Ok(o)
}

/// A relation a publication names, as OpenTableList opened it.
struct OpenRel {
    rel: RelRef,
    where_: Option<pb::Node>,
    cols: Vec<String>,
    /// The row filter as stored (SQL over the table's columns), once transformed.
    qual: Option<String>,
}

/// What OpenTableList found by name.
#[derive(Clone)]
enum RelRef {
    Table(Arc<Table>),
    /// Something else (a view, sequence, index or system catalog): only its errors matter.
    Other {
        oid: u32,
        name: String,
        kind: char,
        system: bool,
    },
}

impl RelRef {
    fn oid(&self) -> u32 {
        match self {
            RelRef::Table(t) => t.oid,
            RelRef::Other { oid, .. } => *oid,
        }
    }

    fn name(&self) -> &str {
        match self {
            RelRef::Table(t) => &t.name,
            RelRef::Other { name, .. } => name,
        }
    }

    fn partitioned(&self) -> bool {
        matches!(self, RelRef::Table(t) if t.partkey.is_some())
    }
}

fn not_part(name: &str) -> Error {
    Error::new(
        code::UNDEFINED_OBJECT,
        format!("relation \"{name}\" is not part of the publication"),
    )
}

/// The object lists of a statement: (tables, schemas).
type Objects = (Vec<pb::PublicationTable>, Vec<u32>);

impl Ddl<'_, '_> {
    pub(super) fn publication_named(&self, name: &str) -> Result<Arc<Publication>> {
        self.cat
            .publications
            .values()
            .find(|p| p.name == name)
            .cloned()
            .ok_or_else(|| {
                Error::new(
                    code::UNDEFINED_OBJECT,
                    format!("publication \"{name}\" does not exist"),
                )
            })
    }

    /// ObjectsInPublicationToOids.
    fn publication_objects(&self, objs: &[pb::Node]) -> Result<Objects> {
        use pb::PublicationObjSpecType as T;
        let (mut tables, mut schemas) = (Vec::new(), Vec::new());
        for o in objs {
            let Some(NodeEnum::PublicationObjSpec(spec)) = inner(o) else {
                continue;
            };
            let schema = match T::try_from(spec.pubobjtype).unwrap_or(T::Undefined) {
                T::PublicationobjTable => {
                    if let Some(t) = &spec.pubtable {
                        tables.push((**t).clone());
                    }
                    continue;
                }
                T::PublicationobjTablesInSchema => self.cat.schema_oid(&spec.name)?,
                T::PublicationobjTablesInCurSchema => {
                    let path = self.cat.effective_path(self.search_path());
                    let Some(s) = self
                        .search_path()
                        .iter()
                        .filter(|s| !s.starts_with("pg_temp"))
                        .find_map(|s| self.cat.schema_by_name(s).map(|x| x.oid))
                        .filter(|s| path.contains(s))
                    else {
                        return Err(Error::new(
                            code::UNDEFINED_SCHEMA,
                            "no schema has been selected for CURRENT_SCHEMA",
                        ));
                    };
                    s
                }
                _ => continue,
            };
            if !schemas.contains(&schema) {
                schemas.push(schema);
            }
        }
        Ok((tables, schemas))
    }

    /// One relation by name (table_openrv): a table, or another kind of relation.
    fn open_rel(&self, rv: &pb::RangeVar) -> Result<RelRef> {
        let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
        if let Some(s) = schema {
            self.require_schema_usage(s)?;
        }
        let other = |oid: u32, name: &str, kind: char| RelRef::Other {
            oid,
            name: name.to_string(),
            kind,
            system: false,
        };
        match self.cat.lookup(schema, &rv.relname, self.search_path())? {
            Some(Relation::Table(t)) => Ok(RelRef::Table(t)),
            Some(Relation::View(v)) => Ok(other(v.oid, &v.name, if v.materialized { 'm' } else { 'v' })),
            Some(Relation::Sequence(q)) => Ok(other(q.oid, &q.name, 'S')),
            Some(Relation::Index(i)) => Ok(other(i.oid, &i.name, 'i')),
            None => match crate::catalog::system::SysTable::lookup(
                schema,
                &rv.relname,
                self.search_path(),
            ) {
                Some(st) => Ok(RelRef::Other {
                    oid: st.oid(),
                    name: st.name().to_string(),
                    kind: if st.is_view() { 'v' } else { 'r' },
                    system: true,
                }),
                None => Err(crate::catalog::no_such_relation(schema, &rv.relname)),
            },
        }
    }

    /// OpenTableList: the named relations and (without ONLY) their inheritance children,
    /// each once, refusing a second WHERE clause or column list for one.
    fn open_table_list(&self, tables: &[pb::PublicationTable]) -> Result<Vec<OpenRel>> {
        let mut out: Vec<OpenRel> = Vec::new();
        let (mut with_rf, mut with_cols): (Vec<u32>, Vec<u32>) = (Vec::new(), Vec::new());
        for t in tables {
            let Some(rv) = &t.relation else { continue };
            let rel = self.open_rel(rv)?;
            let oid = rel.oid();
            let cols: Vec<String> = names(&t.columns);
            let where_ = t.where_clause.as_deref().cloned();
            let dup = |name: &str, what: &str| {
                Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("conflicting or redundant {what} for table \"{name}\""),
                )
            };
            if out.iter().any(|r| r.rel.oid() == oid) {
                if where_.is_some() || with_rf.contains(&oid) {
                    return Err(dup(rel.name(), "WHERE clauses"));
                }
                if !cols.is_empty() || with_cols.contains(&oid) {
                    return Err(dup(rel.name(), "column lists"));
                }
                continue;
            }
            if where_.is_some() {
                with_rf.push(oid);
            }
            if !cols.is_empty() {
                with_cols.push(oid);
            }
            let recurse = rv.inh && !rel.partitioned();
            let name = rel.name().to_string();
            out.push(OpenRel {
                rel,
                where_: where_.clone(),
                cols: cols.clone(),
                qual: None,
            });
            if !recurse {
                continue;
            }
            for child in self.cat.inheritors(oid) {
                if out.iter().any(|r| r.rel.oid() == child) {
                    if child != oid && (where_.is_some() || with_rf.contains(&child)) {
                        return Err(dup(&name, "WHERE clauses"));
                    }
                    if child != oid && (!cols.is_empty() || with_cols.contains(&child)) {
                        return Err(dup(&name, "column lists"));
                    }
                    continue;
                }
                let Some(ct) = self.cat.tables.get(&child).cloned() else {
                    continue;
                };
                if where_.is_some() {
                    with_rf.push(child);
                }
                if !cols.is_empty() {
                    with_cols.push(child);
                }
                out.push(OpenRel {
                    rel: RelRef::Table(ct),
                    where_: where_.clone(),
                    cols: cols.clone(),
                    qual: None,
                });
            }
        }
        Ok(out)
    }

    /// TransformPubWhereClauses: each row filter, checked (check_simple_rowfilter_expr) and
    /// kept as SQL.
    fn transform_where(&self, rels: &mut [OpenRel], via_root: bool) -> Result<()> {
        for r in rels.iter_mut() {
            let Some(w) = r.where_.clone() else { continue };
            let RelRef::Table(t) = &r.rel else {
                continue;
            };
            if !via_root && t.partkey.is_some() {
                return Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("cannot use publication WHERE clause for relation \"{}\"", t.name),
                )
                .detail(
                    "WHERE clause cannot be used for a partitioned table when publish_via_partition_root is false.",
                ));
            }
            let mut b = self.binder();
            // (A system column doesn't bind here; the check names it.)
            let e = match b.bind_over_table(t, &w, Clause::Where) {
                Ok(e) => e,
                Err(e) => {
                    self.check_rowfilter(t, &w)?;
                    return Err(e);
                }
            };
            b.coerce_bool_at(e, "PUBLICATION WHERE", &w)?;
            self.check_rowfilter(t, &w)?;
            r.qual = Some(crate::sql::binder::ast::deparse_expr(&w)?);
        }
        Ok(())
    }

    /// check_simple_rowfilter_expr_walker, over the expression as written: the first node
    /// (outermost, then left to right) that isn't a column, a constant, a built-in operator or
    /// immutable built-in function, of built-in types and collations.
    fn check_rowfilter(&self, t: &Arc<Table>, n: &pb::Node) -> Result<()> {
        let fail = |detail: &str, loc: i32| {
            Err(with_pos(
                Error::new(code::FEATURE_NOT_SUPPORTED, "invalid publication WHERE expression")
                    .detail(detail.to_string()),
                loc,
            ))
        };
        const ONLY: &str = "Only columns, constants, built-in operators, built-in data types, built-in collations, and immutable built-in functions are allowed.";
        let loc = leftmost(n);
        let bound = {
            let mut b = self.binder();
            b.bind_over_table(t, n, Clause::Where).ok()
        };
        let user_type = |e: &crate::sql::expr::Expr| {
            crate::catalog::system::type_oid(&self.cat, &e.ty()) >= FIRST_NORMAL
        };
        let user_coll = |node: &pb::Node| -> bool {
            node.node.as_ref().is_some_and(|ne| {
                ne.nodes().into_iter().any(|(x, ..)| match x {
                    pg_query::NodeRef::CollateClause(c) => {
                        let cn = names(&c.collname);
                        self.collation_is_user(&cn)
                    }
                    _ => false,
                })
            })
        };
        let user_func = |e: &crate::sql::expr::Expr| {
            matches!(e, crate::sql::expr::Expr::Call(d, ..) if d.oid >= FIRST_NORMAL || d.volatility != 'i')
        };
        let Some(node) = inner(n) else { return Ok(()) };
        match node {
            NodeEnum::ColumnRef(c) => {
                let parts = names(&c.fields);
                if let Some(col) = parts.last()
                    && crate::exec::scan::SYS_COLS.iter().any(|(s, _)| s == col)
                    && t.column_index(col).is_none()
                {
                    return fail("System columns are not allowed.", c.location);
                }
            }
            NodeEnum::AExpr(a) => {
                use pb::AExprKind as K;
                let kind = K::try_from(a.kind).unwrap_or(K::Undefined);
                let opname = names(&a.name).last().cloned().unwrap_or_default();
                let is_op = matches!(
                    kind,
                    K::AexprOp | K::AexprOpAny | K::AexprOpAll | K::AexprDistinct | K::AexprNotDistinct | K::AexprNullif | K::AexprIn | K::AexprLike | K::AexprIlike | K::AexprSimilar
                );
                if is_op && self.cat.operators.values().any(|o| o.name == opname) {
                    // A user-defined operator of this name that the arguments resolve to.
                    let mut b = self.binder();
                    let lt = a.lexpr.as_deref().and_then(|l| b.bind_over_table(t, l, Clause::Where).ok()).map(|e| e.ty());
                    let rt = a.rexpr.as_deref().and_then(|r| b.bind_over_table(t, r, Clause::Where).ok()).map(|e| e.ty());
                    if self.cat.operators.values().any(|o| {
                        o.name == opname
                            && o.left.as_ref() == lt.as_ref()
                            && rt.as_ref().is_none_or(|r| *r == o.right)
                    }) {
                        return fail("User-defined operators are not allowed.", loc);
                    }
                }
                if matches!(kind, K::AexprBetween | K::AexprNotBetween | K::AexprBetweenSym | K::AexprNotBetweenSym) {
                    // A BoolExpr of comparisons.
                } else if let Some(e) = &bound {
                    if user_type(e) {
                        return fail("User-defined types are not allowed.", loc);
                    }
                    if user_func(e) {
                        return fail("User-defined or built-in mutable functions are not allowed.", loc);
                    }
                }
                if is_op && user_coll(n) {
                    return fail("User-defined collations are not allowed.", loc);
                }
                for x in [a.lexpr.as_deref(), a.rexpr.as_deref()].into_iter().flatten() {
                    // IN's list is a List of the elements.
                    if let Some(NodeEnum::List(l)) = inner(x) {
                        for i in &l.items {
                            self.check_rowfilter(t, i)?;
                        }
                    } else {
                        self.check_rowfilter(t, x)?;
                    }
                }
                return Ok(());
            }
            NodeEnum::FuncCall(f) => {
                let fname = names(&f.funcname).last().cloned().unwrap_or_default();
                let user = self
                    .cat
                    .functions
                    .values()
                    .any(|u| u.name == fname && u.args.len() == f.args.len());
                if user || bound.as_ref().is_some_and(user_func) {
                    return fail("User-defined or built-in mutable functions are not allowed.", f.location);
                }
                if bound.as_ref().is_some_and(user_type) {
                    return fail("User-defined types are not allowed.", f.location);
                }
                if user_coll(n) {
                    return fail("User-defined collations are not allowed.", f.location);
                }
                for x in &f.args {
                    self.check_rowfilter(t, x)?;
                }
                return Ok(());
            }
            NodeEnum::CollateClause(c) => {
                if self.collation_is_user(&names(&c.collname)) {
                    return fail("User-defined collations are not allowed.", c.location);
                }
            }
            NodeEnum::AConst(_)
            | NodeEnum::BoolExpr(_)
            | NodeEnum::TypeCast(_)
            | NodeEnum::CaseExpr(_)
            | NodeEnum::CaseWhen(_)
            | NodeEnum::AArrayExpr(_)
            | NodeEnum::RowExpr(_)
            | NodeEnum::CoalesceExpr(_)
            | NodeEnum::MinMaxExpr(_)
            | NodeEnum::XmlExpr(_)
            | NodeEnum::NullTest(_)
            | NodeEnum::BooleanTest(_)
            | NodeEnum::List(_) => {}
            _ => return fail(ONLY, loc),
        }
        if let Some(e) = &bound
            && !matches!(node, NodeEnum::List(_) | NodeEnum::CaseWhen(_))
            && user_type(e)
        {
            return fail("User-defined types are not allowed.", loc);
        }
        // The children, in order.
        let kids: Vec<pb::Node> = match node {
            NodeEnum::BoolExpr(b) => b.args.clone(),
            NodeEnum::TypeCast(c) => c.arg.as_deref().cloned().into_iter().collect(),
            NodeEnum::CollateClause(c) => c.arg.as_deref().cloned().into_iter().collect(),
            NodeEnum::CaseExpr(c) => {
                let mut v: Vec<pb::Node> = c.arg.as_deref().cloned().into_iter().collect();
                for w in &c.args {
                    if let Some(NodeEnum::CaseWhen(cw)) = inner(w) {
                        v.extend(cw.expr.as_deref().cloned());
                        v.extend(cw.result.as_deref().cloned());
                    }
                }
                v.extend(c.defresult.as_deref().cloned());
                v
            }
            NodeEnum::AArrayExpr(a) => a.elements.clone(),
            NodeEnum::RowExpr(r) => r.args.clone(),
            NodeEnum::CoalesceExpr(c) => c.args.clone(),
            NodeEnum::MinMaxExpr(m) => m.args.clone(),
            NodeEnum::XmlExpr(x) => x.args.iter().chain(&x.named_args).cloned().collect(),
            NodeEnum::NullTest(x) => x.arg.as_deref().cloned().into_iter().collect(),
            NodeEnum::BooleanTest(x) => x.arg.as_deref().cloned().into_iter().collect(),
            NodeEnum::List(l) => l.items.clone(),
            _ => Vec::new(),
        };
        for k in &kids {
            // Constant-folded literal casts are just constants.
            self.check_rowfilter(t, k)?;
        }
        Ok(())
    }

    fn collation_is_user(&self, name: &[String]) -> bool {
        let last = name.last().map(String::as_str).unwrap_or("");
        self.cat.collations.values().any(|c| c.name == last)
    }

    /// CheckPubRelationColumnList.
    fn check_column_lists(
        &self,
        pubname: &str,
        rels: &[OpenRel],
        schemas: bool,
        via_root: bool,
    ) -> Result<()> {
        for r in rels.iter().filter(|r| !r.cols.is_empty()) {
            let RelRef::Table(t) = &r.rel else { continue };
            let err = |detail: String| {
                Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!(
                        "cannot use column list for relation \"{}.{}\" in publication \"{pubname}\"",
                        self.cat.schema_name(t.schema),
                        t.name
                    ),
                )
                .detail(detail)
            };
            if schemas {
                return Err(err(
                    "Column lists cannot be specified in publications containing FOR TABLES IN SCHEMA elements."
                        .into(),
                ));
            }
            if !via_root && t.partkey.is_some() {
                return Err(err(
                    "Column lists cannot be specified for partitioned tables when publish_via_partition_root is false."
                        .into(),
                ));
            }
        }
        Ok(())
    }

    /// pub_collist_validate: the column list as attribute numbers, ascending.
    fn column_list_attnums(&self, t: &Table, cols: &[String]) -> Result<Option<Vec<i16>>> {
        if cols.is_empty() {
            return Ok(None);
        }
        let mut set: Vec<i16> = Vec::new();
        for c in cols {
            let Some(i) = t.column_index(c) else {
                if crate::exec::scan::SYS_COLS.iter().any(|(s, _)| s == c) {
                    return Err(Error::new(
                        code::INVALID_COLUMN_REFERENCE,
                        format!("cannot use system column \"{c}\" in publication column list"),
                    ));
                }
                return Err(Error::new(
                    code::UNDEFINED_COLUMN,
                    format!("column \"{c}\" of relation \"{}\" does not exist", t.name),
                ));
            };
            if t.columns[i].virtual_gen {
                return Err(Error::new(
                    code::INVALID_COLUMN_REFERENCE,
                    format!("cannot use virtual generated column \"{c}\" in publication column list"),
                ));
            }
            let a = i as i16 + 1;
            if set.contains(&a) {
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("duplicate column \"{c}\" in publication column list"),
                ));
            }
            set.push(a);
        }
        set.sort_unstable();
        Ok(Some(set))
    }

    /// check_publication_add_relation.
    fn check_add_relation(&self, r: &RelRef) -> Result<()> {
        let name = r.name();
        let err = |detail: &str| {
            Err(Error::new(
                code::INVALID_PARAMETER_VALUE,
                format!("cannot add relation \"{name}\" to publication"),
            )
            .detail(detail.to_string()))
        };
        match r {
            RelRef::Other { system: true, kind: 'r', .. } => {
                err("This operation is not supported for system tables.")
            }
            RelRef::Other { kind, .. } => err(&format!(
                "This operation is not supported for {}.",
                super::foreign::relkind_plural(*kind)
            )),
            RelRef::Table(t) if t.foreign.is_some() => {
                err("This operation is not supported for foreign tables.")
            }
            RelRef::Table(t) if t.temporary => {
                err("This operation is not supported for temporary tables.")
            }
            RelRef::Table(t) if t.unlogged => {
                err("This operation is not supported for unlogged tables.")
            }
            RelRef::Table(_) => Ok(()),
        }
    }

    /// PublicationAddTables: ownership of each, then publication_add_relation.
    fn add_tables(&mut self, p: &mut Publication, rels: &[OpenRel], if_not_exists: bool) -> Result<()> {
        for r in rels {
            if let RelRef::Table(t) = &r.rel {
                self.require_owner(
                    t.owner,
                    if t.foreign.is_some() { "foreign table" } else { "table" },
                    &t.name,
                )?;
            } else if let RelRef::Other { system: false, oid, name, .. } = &r.rel {
                self.require_rel_owner(*oid, "relation", name)?;
            }
            let oid = r.rel.oid();
            if p.rels.iter().any(|x| x.rel == oid) {
                if if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!(
                        "relation \"{}\" is already member of publication \"{}\"",
                        r.rel.name(),
                        p.name
                    ),
                ));
            }
            self.check_add_relation(&r.rel)?;
            let RelRef::Table(t) = &r.rel else { continue };
            let cols = self.column_list_attnums(t, &r.cols)?;
            let roid = self.cat.alloc_oid();
            p.rels.push(PubRel {
                oid: roid,
                rel: t.oid,
                qual: r.qual.clone(),
                cols,
            });
        }
        Ok(())
    }

    /// PublicationDropTables.
    fn drop_tables(&mut self, p: &mut Publication, rels: &[OpenRel], missing_ok: bool) -> Result<()> {
        for r in rels {
            if !r.cols.is_empty() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "column list must not be specified in ALTER PUBLICATION ... DROP",
                ));
            }
            let oid = r.rel.oid();
            if !p.rels.iter().any(|x| x.rel == oid) {
                if missing_ok {
                    continue;
                }
                return Err(not_part(r.rel.name()));
            }
            if r.where_.is_some() {
                return Err(Error::new(
                    code::SYNTAX_ERROR,
                    "cannot use a WHERE clause when removing a table from a publication",
                ));
            }
            p.rels.retain(|x| x.rel != oid);
        }
        Ok(())
    }

    /// check_publication_add_schema, then the row (publication_add_schema).
    fn add_schemas(&mut self, p: &mut Publication, schemas: &[u32], if_not_exists: bool) -> Result<()> {
        for s in schemas {
            let name = self.cat.schema_name(*s).to_string();
            if p.schemas.iter().any(|x| x.schema == *s) {
                if if_not_exists {
                    continue;
                }
                return Err(Error::new(
                    code::DUPLICATE_OBJECT,
                    format!("schema \"{name}\" is already member of publication \"{}\"", p.name),
                ));
            }
            let err = |detail: &str| {
                Err(Error::new(
                    code::INVALID_PARAMETER_VALUE,
                    format!("cannot add schema \"{name}\" to publication"),
                )
                .detail(detail.to_string()))
            };
            if *s == crate::catalog::PG_CATALOG_OID || name.starts_with("pg_toast") {
                return err("This operation is not supported for system schemas.");
            }
            if name.starts_with("pg_temp") {
                return err("Temporary schemas cannot be replicated.");
            }
            let oid = self.cat.alloc_oid();
            p.schemas.push(PubSchema { oid, schema: *s });
        }
        Ok(())
    }

    fn require_publication_owner(&self, p: &Publication) -> Result<()> {
        self.require_owner(p.owner, "publication", &p.name)
    }

    /// CREATE PUBLICATION.
    pub(super) fn create_publication(&mut self, s: &pb::CreatePublicationStmt) -> Result<String> {
        self.require_database_priv(acl::CREATE)?;
        if s.for_all_tables && !self.is_superuser() {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to create FOR ALL TABLES publication",
            ));
        }
        if self.cat.publications.values().any(|p| p.name == s.pubname) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("publication \"{}\" already exists", s.pubname),
            ));
        }
        let o = parse_options(&s.options)?;
        let (ins, upd, del, tru) = o.publish.unwrap_or((true, true, true, true));
        let oid = self.cat.alloc_oid();
        let mut p = Publication {
            oid,
            name: s.pubname.clone(),
            owner: self.me(),
            all_tables: s.for_all_tables,
            insert: ins,
            update: upd,
            delete: del,
            truncate: tru,
            via_root: o.via_root.unwrap_or(false),
            gencols: o.gencols.unwrap_or('n'),
            rels: Vec::new(),
            schemas: Vec::new(),
        };
        if !s.for_all_tables {
            let (tables, schemas) = self.publication_objects(&s.pubobjects)?;
            if !schemas.is_empty() && !self.is_superuser() {
                return Err(Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    "must be superuser to create FOR TABLES IN SCHEMA publication",
                ));
            }
            if !tables.is_empty() {
                let mut rels = self.open_table_list(&tables)?;
                self.transform_where(&mut rels, p.via_root)?;
                self.check_column_lists(&p.name, &rels, !schemas.is_empty(), p.via_root)?;
                self.add_tables(&mut p, &rels, true)?;
            }
            self.add_schemas(&mut p, &schemas, true)?;
        }
        self.cat.publications.insert(oid, Arc::new(p));
        self.save()?;
        let wal = self
            .ctx
            .sess
            .settings
            .borrow()
            .get_raw("wal_level")
            .unwrap_or_default();
        if wal != "logical" {
            self.ctx.notice(
                crate::session::warning(
                    code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                    "\"wal_level\" is insufficient to publish logical changes",
                )
                .hint("Set \"wal_level\" to \"logical\" before creating subscriptions."),
            );
        }
        Ok("CREATE PUBLICATION".into())
    }

    /// ALTER PUBLICATION ... SET (options) / ADD / DROP / SET objects.
    pub(super) fn alter_publication(&mut self, s: &pb::AlterPublicationStmt) -> Result<String> {
        const TAG: &str = "ALTER PUBLICATION";
        let p = self.publication_named(&s.pubname)?;
        self.require_publication_owner(&p)?;
        let mut p = (*p).clone();
        if !s.options.is_empty() {
            let o = parse_options(&s.options)?;
            if !p.all_tables && o.via_root == Some(false) {
                for r in &p.rels {
                    let Some(t) = self.cat.tables.get(&r.rel) else { continue };
                    if t.partkey.is_none() || (r.qual.is_none() && r.cols.is_none()) {
                        continue;
                    }
                    let what = if r.qual.is_some() { "a WHERE clause" } else { "a column list" };
                    return Err(Error::new(
                        code::INVALID_PARAMETER_VALUE,
                        format!(
                            "cannot set parameter \"publish_via_partition_root\" to false for publication \"{}\"",
                            p.name
                        ),
                    )
                    .detail(format!(
                        "The publication contains {what} for partitioned table \"{}\", which is not allowed when \"publish_via_partition_root\" is false.",
                        t.name
                    )));
                }
            }
            if let Some((i, u, d, t)) = o.publish {
                (p.insert, p.update, p.delete, p.truncate) = (i, u, d, t);
            }
            if let Some(v) = o.via_root {
                p.via_root = v;
            }
            if let Some(g) = o.gencols {
                p.gencols = g;
            }
            self.cat.publications.insert(p.oid, Arc::new(p));
            self.save()?;
            return Ok(TAG.into());
        }
        use pb::AlterPublicationAction as A;
        let action = A::try_from(s.action).unwrap_or(A::Undefined);
        let (tables, schemas) = self.publication_objects(&s.pubobjects)?;
        // CheckAlterPublication.
        if matches!(action, A::ApAddObjects | A::ApSetObjects)
            && !schemas.is_empty()
            && !self.is_superuser()
        {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "must be superuser to add or set schemas",
            ));
        }
        let all = |what: &str| {
            Error::new(
                code::OBJECT_NOT_IN_PREREQUISITE_STATE,
                format!("publication \"{}\" is defined as FOR ALL TABLES", p.name),
            )
            .detail(format!(
                "{what} cannot be added to or dropped from FOR ALL TABLES publications."
            ))
        };
        if !schemas.is_empty() && p.all_tables {
            return Err(all("Schemas"));
        }
        if !tables.is_empty() && p.all_tables {
            return Err(all("Tables"));
        }
        // AlterPublicationTables.
        if !tables.is_empty() || action == A::ApSetObjects {
            let mut rels = self.open_table_list(&tables)?;
            match action {
                A::ApAddObjects => {
                    self.transform_where(&mut rels, p.via_root)?;
                    let schema_pub = !schemas.is_empty() || !p.schemas.is_empty();
                    self.check_column_lists(&p.name, &rels, schema_pub, p.via_root)?;
                    self.add_tables(&mut p, &rels, false)?;
                }
                A::ApDropObjects => self.drop_tables(&mut p, &rels, false)?,
                _ => {
                    self.transform_where(&mut rels, p.via_root)?;
                    self.check_column_lists(&p.name, &rels, !schemas.is_empty(), p.via_root)?;
                    // Keep what is named again as it is; drop the rest.
                    let mut keep = Vec::new();
                    for old in &p.rels {
                        let mut found = false;
                        for r in &rels {
                            let RelRef::Table(t) = &r.rel else { continue };
                            let cols = self.column_list_attnums(t, &r.cols)?;
                            if t.oid == old.rel && r.qual == old.qual && cols == old.cols {
                                found = true;
                                break;
                            }
                        }
                        if found {
                            keep.push(old.rel);
                        }
                    }
                    p.rels.retain(|x| keep.contains(&x.rel));
                    self.add_tables(&mut p, &rels, true)?;
                }
            }
        }
        // AlterPublicationSchemas.
        if !schemas.is_empty() || action == A::ApSetObjects {
            match action {
                A::ApAddObjects => {
                    if p.rels.iter().any(|r| r.cols.is_some()) {
                        return Err(Error::new(
                            code::INVALID_PARAMETER_VALUE,
                            format!("cannot add schema to publication \"{}\"", p.name),
                        )
                        .detail(
                            "Schemas cannot be added if any tables that specify a column list are already part of the publication.",
                        ));
                    }
                    self.add_schemas(&mut p, &schemas, false)?;
                }
                A::ApDropObjects => {
                    for sc in &schemas {
                        if !p.schemas.iter().any(|x| x.schema == *sc) {
                            return Err(Error::new(
                                code::UNDEFINED_OBJECT,
                                format!(
                                    "tables from schema \"{}\" are not part of the publication",
                                    self.cat.schema_name(*sc)
                                ),
                            ));
                        }
                        p.schemas.retain(|x| x.schema != *sc);
                    }
                }
                _ => {
                    p.schemas.retain(|x| schemas.contains(&x.schema));
                    self.add_schemas(&mut p, &schemas, true)?;
                }
            }
        }
        self.cat.publications.insert(p.oid, Arc::new(p));
        self.save()?;
        Ok(TAG.into())
    }

    /// DROP PUBLICATION: the publication to drop (None: missing, IF EXISTS).
    pub(super) fn publication_to_drop(&self, name: &str, missing_ok: bool) -> Result<Option<u32>> {
        match self.cat.publications.values().find(|p| p.name == name) {
            Some(p) => {
                self.require_publication_owner(p)?;
                Ok(Some(p.oid))
            }
            None if missing_ok => {
                self.notice(format!("publication \"{name}\" does not exist, skipping"));
                Ok(None)
            }
            None => Err(Error::new(
                code::UNDEFINED_OBJECT,
                format!("publication \"{name}\" does not exist"),
            )),
        }
    }

    /// ALTER PUBLICATION ... RENAME TO.
    pub(super) fn rename_publication(&mut self, old: &str, new: &str) -> Result<String> {
        let p = self.publication_named(old)?;
        self.require_publication_owner(&p)?;
        if self.cat.publications.values().any(|x| x.name == new) {
            return Err(Error::new(
                code::DUPLICATE_OBJECT,
                format!("publication \"{new}\" already exists"),
            ));
        }
        let mut p = (*p).clone();
        p.name = new.to_string();
        self.cat.publications.insert(p.oid, Arc::new(p));
        self.save()?;
        Ok("ALTER PUBLICATION".into())
    }

    /// ALTER PUBLICATION ... OWNER TO (AlterPublicationOwner_internal).
    pub(super) fn owner_publication(&mut self, name: &str, new: u32) -> Result<()> {
        let p = self.publication_named(name)?;
        if p.owner == new {
            return Ok(());
        }
        if !self.is_superuser() {
            self.require_publication_owner(&p)?;
            self.check_new_owner(new, None)?;
            // The new owner needs CREATE on the database.
            let db = self.ctx.sess.database.clone();
            if let Some((oid, _, _)) = self
                .ctx
                .sess
                .cluster
                .databases()
                .into_iter()
                .find(|d| d.1 == db)
            {
                let (privs, _) = self.cat.obj_mask(ObjKind::Database, oid, new, self.db_owner());
                if privs & acl::CREATE == 0 && !self.cat.is_superuser(new) {
                    return Err(Error::new(
                        code::INSUFFICIENT_PRIVILEGE,
                        format!("permission denied for database {db}"),
                    ));
                }
            }
            let denied = |hint: &str| {
                Error::new(
                    code::INSUFFICIENT_PRIVILEGE,
                    format!("permission denied to change owner of publication \"{}\"", p.name),
                )
                .hint(hint.to_string())
            };
            if p.all_tables && !self.cat.is_superuser(new) {
                return Err(denied("The owner of a FOR ALL TABLES publication must be a superuser."));
            }
            if !p.schemas.is_empty() && !self.cat.is_superuser(new) {
                return Err(denied(
                    "The owner of a FOR TABLES IN SCHEMA publication must be a superuser.",
                ));
            }
        }
        let mut p = (*p).clone();
        p.owner = new;
        self.cat.publications.insert(p.oid, Arc::new(p));
        Ok(())
    }

    /// deleteOneObject for a publication, one of its tables or one of its schemas.
    pub(super) fn delete_publication_object(&mut self, class: u32, oid: u32) {
        match class {
            crate::catalog::depend::PUBLICATION => {
                self.cat.publications.remove(&oid);
            }
            _ => {
                let owner = self
                    .cat
                    .publications
                    .values()
                    .find(|p| p.rels.iter().any(|r| r.oid == oid) || p.schemas.iter().any(|s| s.oid == oid))
                    .cloned();
                if let Some(p) = owner {
                    let mut p = (*p).clone();
                    p.rels.retain(|r| r.oid != oid);
                    p.schemas.retain(|s| s.oid != oid);
                    self.cat.publications.insert(p.oid, Arc::new(p));
                }
            }
        }
        self.cat.comments.retain(|(o, _, _)| *o != oid);
    }
}

/// exprLocation of an expression as written: an operator's leftmost operand's position when
/// that comes first.
fn leftmost(n: &pb::Node) -> i32 {
    match inner(n) {
        Some(NodeEnum::AExpr(a)) => {
            let l = a.lexpr.as_deref().map_or(-1, leftmost);
            if l >= 0 && (a.location < 0 || l < a.location) { l } else { a.location }
        }
        Some(NodeEnum::SubLink(s)) => {
            let l = s.testexpr.as_deref().map_or(-1, leftmost);
            if l >= 0 && (s.location < 0 || l < s.location) { l } else { s.location }
        }
        Some(NodeEnum::TypeCast(c)) => {
            let l = c.arg.as_deref().map_or(-1, leftmost);
            if l >= 0 && (c.location < 0 || l < c.location) { l } else { c.location }
        }
        Some(NodeEnum::CollateClause(c)) => {
            let l = c.arg.as_deref().map_or(-1, leftmost);
            if l >= 0 && (c.location < 0 || l < c.location) { l } else { c.location }
        }
        _ => crate::sql::binder::ast::location(n),
    }
}
