//! UPDATE of pg_class by a superuser, for the columns tests poke to steer the planner
//! (`reltuples`, `relpages`, `relallvisible`): the rows to change are found by querying
//! pg_class, and the new values go into the relations' stored statistics.

use super::Ddl;
use crate::catalog::RelStats;
use crate::error::{Error, Result, code};
use crate::sql::binder::Binder;
use crate::sql::binder::ast::{inner, mk};
use crate::types::Value;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// Whether `s` updates pg_class or pg_database (unqualified or in pg_catalog).
pub(crate) fn is_pg_class_update(s: &pb::UpdateStmt) -> bool {
    s.relation.as_ref().is_some_and(|rv| {
        matches!(rv.relname.as_str(), "pg_class" | "pg_database")
            && (rv.schemaname.is_empty() || rv.schemaname == "pg_catalog")
    }) && s.with_clause.is_none()
        && crate::sql::binder::ast::returning(&s.returning_clause).is_empty()
        && s.from_clause.is_empty()
}

fn col(name: &str) -> pb::Node {
    mk(NodeEnum::ColumnRef(pb::ColumnRef {
        fields: vec![mk(NodeEnum::String(pb::String {
            sval: name.to_string(),
        }))],
        location: -1,
    }))
}

fn target(val: pb::Node) -> pb::Node {
    mk(NodeEnum::ResTarget(Box::new(pb::ResTarget {
        name: String::new(),
        indirection: Vec::new(),
        val: Some(Box::new(val)),
        location: -1,
    })))
}

fn cast(val: pb::Node, ty: &str) -> pb::Node {
    mk(NodeEnum::TypeCast(Box::new(pb::TypeCast {
        arg: Some(Box::new(val)),
        type_name: Some(pb::TypeName {
            names: ["pg_catalog", ty]
                .iter()
                .map(|n| {
                    mk(NodeEnum::String(pb::String {
                        sval: n.to_string(),
                    }))
                })
                .collect(),
            typemod: -1,
            location: -1,
            ..Default::default()
        }),
        location: -1,
    })))
}

impl Ddl<'_, '_> {
    pub(super) fn update_pg_class(&mut self, s: &pb::UpdateStmt) -> Result<String> {
        if s.relation.as_ref().is_some_and(|rv| rv.relname == "pg_database") {
            return self.update_pg_database(s);
        }
        let me = self.ctx.sess.current_role(&self.cat);
        if !self.cat.is_superuser(me) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied for table pg_class",
            ));
        }
        // (column, its type, the new value).
        let mut sets: Vec<(&str, pb::Node)> = Vec::new();
        for t in &s.target_list {
            let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                continue;
            };
            let ty = match rt.name.as_str() {
                "reltuples" => "float4",
                "relpages" | "relallvisible" | "relallfrozen" => "int4",
                "reloptions" => "_text",
                other => {
                    return Err(crate::error::unsupported(format!(
                        "UPDATE of pg_class.{other}"
                    )));
                }
            };
            let val = rt.val.as_deref().cloned().unwrap_or_else(|| {
                mk(NodeEnum::AConst(pb::AConst {
                    isnull: true,
                    val: None,
                    location: -1,
                }))
            });
            sets.push((rt.name.as_str(), cast(val, ty)));
        }
        let mut targets = vec![
            target(col("oid")),
            target(col("relpages")),
            target(col("reltuples")),
            target(col("relallvisible")),
        ];
        targets.extend(sets.iter().map(|(_, v)| target(v.clone())));
        let sel = pb::SelectStmt {
            target_list: targets,
            from_clause: vec![mk(NodeEnum::RangeVar(s.relation.clone().unwrap()))],
            where_clause: s.where_clause.clone(),
            ..Default::default()
        };
        let node = mk(NodeEnum::SelectStmt(Box::new(sel)));
        let child = self.ctx.child(&[]);
        let rows = {
            let mut b = Binder::new(&child, Vec::new());
            let bq = b.bind_statement(&node)?;
            crate::exec::collect(&bq.plan, &child, None)?
        };
        let num = |v: &Value| match v {
            Value::Int4(i) => *i as f64,
            Value::Float4(f) => *f as f64,
            Value::Null => 0.0,
            other => other.as_f64().unwrap_or(0.0),
        };
        for r in &rows {
            let Value::Oid(oid) = r[0] else { continue };
            let mut st = RelStats {
                pages: num(&r[1]) as i32,
                tuples: num(&r[2]),
                allvisible: num(&r[3]) as i32,
                allfrozen: self.cat.relstats.get(&oid).map_or(0, |s| s.allfrozen),
            };
            let mut stats = false;
            for (i, (name, _)) in sets.iter().enumerate() {
                let v = &r[4 + i];
                if *name == "reloptions" {
                    // (Stored as given: Postgres doesn't check a direct catalog update.)
                    let opts: Vec<String> = match v {
                        Value::Array(a) => a
                            .items
                            .iter()
                            .filter(|x| !x.is_null())
                            .map(|x| match x {
                                Value::Text(t) => t.to_string(),
                                other => crate::types::text::format_value(
                                    other,
                                    &crate::types::DataType::Text,
                                    &Default::default(),
                                ),
                            })
                            .collect(),
                        _ => Vec::new(),
                    };
                    if let Some(t) = self.cat.tables.get(&oid).cloned() {
                        let mut t = (*t).clone();
                        t.options = opts;
                        self.cat.put_storage(t);
                    } else if let Some(ix) = self.cat.indexes.get(&oid).cloned() {
                        let mut ix = (*ix).clone();
                        ix.options = opts;
                        std::sync::Arc::make_mut(&mut self.cat.indexes).insert(oid, std::sync::Arc::new(ix));
                    }
                    continue;
                }
                stats = true;
                if v.is_null() {
                    return Err(Error::new(
                        code::NOT_NULL_VIOLATION,
                        format!(
                            "null value in column \"{name}\" of relation \"pg_class\" violates not-null constraint"
                        ),
                    ));
                }
                match *name {
                    "reltuples" => st.tuples = num(v),
                    "relpages" => st.pages = num(v) as i32,
                    "relallvisible" => st.allvisible = num(v) as i32,
                    "relallfrozen" => st.allfrozen = num(v) as i32,
                    _ => {}
                }
            }
            if stats && oid >= crate::catalog::FIRST_USER_OID {
                std::sync::Arc::make_mut(&mut self.cat.relstats).insert(oid, st);
            }
        }
        self.save()?;
        Ok(format!("UPDATE {}", rows.len()))
    }
}

impl Ddl<'_, '_> {
    /// UPDATE pg_database SET datacl = ...: a superuser's direct change of databases' ACLs.
    fn update_pg_database(&mut self, s: &pb::UpdateStmt) -> Result<String> {
        let me = self.ctx.sess.current_role(&self.cat);
        if !self.cat.is_superuser(me) {
            return Err(Error::new(
                code::INSUFFICIENT_PRIVILEGE,
                "permission denied for table pg_database",
            ));
        }
        let mut targets = vec![target(col("oid"))];
        for t in &s.target_list {
            let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                continue;
            };
            if rt.name != "datacl" {
                return Err(crate::error::unsupported(format!(
                    "UPDATE of pg_database.{}",
                    rt.name
                )));
            }
            let val = rt.val.as_deref().cloned().unwrap_or_else(|| {
                mk(NodeEnum::AConst(pb::AConst {
                    isnull: true,
                    val: None,
                    location: -1,
                }))
            });
            targets.push(target(cast(val, "_aclitem")));
        }
        let sel = pb::SelectStmt {
            target_list: targets,
            from_clause: vec![mk(NodeEnum::RangeVar(s.relation.clone().unwrap()))],
            where_clause: s.where_clause.clone(),
            ..Default::default()
        };
        let node = mk(NodeEnum::SelectStmt(Box::new(sel)));
        let child = self.ctx.child(&[]);
        let rows = {
            let mut b = Binder::new(&child, Vec::new());
            let bq = b.bind_statement(&node)?;
            crate::exec::collect(&bq.plan, &child, None)?
        };
        for r in &rows {
            let Value::Oid(oid) = r[0] else { continue };
            for v in &r[1..] {
                let acl = match v {
                    Value::Array(a) => {
                        let mut acl = Vec::with_capacity(a.items.len());
                        for x in a.items.iter().filter(|x| !x.is_null()) {
                            let text = match x {
                                Value::Text(t) => t.to_string(),
                                _ => continue,
                            };
                            let (item, _) = crate::catalog::acl::parse_item(&text, &|n| {
                                self.cat.role_by_name(n).map(|r| r.oid)
                            })?;
                            acl.push(item);
                        }
                        Some(acl)
                    }
                    _ => None,
                };
                self.cat
                    .set_acl(crate::catalog::acl::ObjKind::Database, oid, 0, acl);
            }
        }
        self.save()?;
        Ok(format!("UPDATE {}", rows.len()))
    }
}

// ── pg_depend ──

/// pg_depend's columns, in order.
const DEPEND_COLS: [&str; 7] = [
    "classid",
    "objid",
    "objsubid",
    "refclassid",
    "refobjid",
    "refobjsubid",
    "deptype",
];

fn is_pg_depend(rv: &pb::RangeVar) -> bool {
    rv.relname == "pg_depend" && (rv.schemaname.is_empty() || rv.schemaname == "pg_catalog")
}

/// Whether `s` deletes from pg_depend (unqualified or in pg_catalog).
pub(crate) fn is_pg_depend_delete(s: &pb::DeleteStmt) -> bool {
    s.relation.as_ref().is_some_and(is_pg_depend) && s.with_clause.is_none()
}

/// Whether `s` inserts into pg_depend.
pub(crate) fn is_pg_depend_insert(s: &pb::InsertStmt) -> bool {
    s.relation.as_ref().is_some_and(is_pg_depend)
        && s.with_clause.is_none()
        && s.on_conflict_clause.is_none()
        && s.select_stmt.is_some()
}

fn qcol(rel: &str, name: &str) -> pb::Node {
    mk(NodeEnum::ColumnRef(pb::ColumnRef {
        fields: [rel, name]
            .iter()
            .map(|n| {
                mk(NodeEnum::String(pb::String {
                    sval: n.to_string(),
                }))
            })
            .collect(),
        location: -1,
    }))
}

/// A pg_depend row out of its seven column values.
fn depend_row(r: &[Value]) -> Option<crate::catalog::DependRow> {
    let oid = |v: &Value| match v {
        Value::Oid(o) => Some(*o),
        _ => None,
    };
    let int = |v: &Value| match v {
        Value::Int4(i) => Some(*i),
        _ => None,
    };
    let kind = match &r[6] {
        Value::Char(c) => *c as char,
        Value::Text(s) => s.chars().next().unwrap_or('\0'),
        _ => return None,
    };
    Some((
        (oid(&r[0])?, oid(&r[1])?, int(&r[2])?),
        (oid(&r[3])?, oid(&r[4])?, int(&r[5])?),
        kind,
    ))
}

/// What a statement on pg_depend returned: RETURNING's columns and rows, and its tag.
pub(crate) type DependDml = (Vec<crate::sql::plan::OutCol>, Vec<crate::exec::Row>, String);

/// DELETE FROM / INSERT INTO pg_depend by a superuser. pg_depend is derived from the
/// objects; the rows a DELETE finds are masked and the rows an INSERT gives added, over
/// it (`Catalog::depend_edits`), so dependency checks see them as Postgres's would.
pub(crate) fn pg_depend_dml(ctx: &crate::exec::ExecCtx, node: &NodeEnum) -> Result<DependDml> {
    let cat = ctx.catalog();
    let me = ctx.sess.current_role(&cat);
    if !cat.is_superuser(me) {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_depend",
        ));
    }
    let child = ctx.child(&[]);
    let run = |sel: pb::SelectStmt| -> Result<(Vec<crate::sql::plan::OutCol>, Vec<crate::exec::Row>)> {
        let node = mk(NodeEnum::SelectStmt(Box::new(sel)));
        let mut b = Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&node)?;
        let rows = crate::exec::collect(&bq.plan, &child, None)?;
        Ok((bq.cols, rows))
    };
    let mut d = Ddl::new(ctx, "");
    match node {
        NodeEnum::DeleteStmt(s) => {
            let rv = s.relation.clone().unwrap();
            let rel = rv
                .alias
                .as_ref()
                .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
            let mut targets: Vec<pb::Node> =
                DEPEND_COLS.iter().map(|c| target(qcol(&rel, c))).collect();
            targets.extend(crate::sql::binder::ast::returning(&s.returning_clause).iter().cloned());
            let mut from = vec![mk(NodeEnum::RangeVar(rv))];
            from.extend(s.using_clause.iter().cloned());
            let (cols, rows) = run(pb::SelectStmt {
                target_list: targets,
                from_clause: from,
                where_clause: s.where_clause.clone(),
                ..Default::default()
            })?;
            // (A row USING joins more than once is deleted once.)
            let mut seen: Vec<crate::catalog::DependRow> = Vec::new();
            let mut out = Vec::new();
            for r in rows {
                let Some(row) = depend_row(&r) else { continue };
                if !s.using_clause.is_empty() && seen.contains(&row) {
                    continue;
                }
                seen.push(row);
                d.cat.depend_edits.remove(row);
                out.push(r[DEPEND_COLS.len()..].to_vec());
            }
            d.save()?;
            let cols = cols[DEPEND_COLS.len()..].to_vec();
            Ok((cols, out, format!("DELETE {}", seen.len())))
        }
        NodeEnum::InsertStmt(s) => {
            if !crate::sql::binder::ast::returning(&s.returning_clause).is_empty() {
                return Err(crate::error::unsupported("INSERT INTO pg_depend ... RETURNING"));
            }
            // The columns given, in pg_depend's order of them.
            let names: Vec<String> = if s.cols.is_empty() {
                DEPEND_COLS.iter().map(|c| c.to_string()).collect()
            } else {
                s.cols
                    .iter()
                    .filter_map(|c| match inner(c) {
                        Some(NodeEnum::ResTarget(rt)) => Some(rt.name.clone()),
                        _ => None,
                    })
                    .collect()
            };
            for n in &names {
                if !DEPEND_COLS.contains(&n.as_str()) {
                    return Err(Error::new(
                        code::UNDEFINED_COLUMN,
                        format!("column \"{n}\" of relation \"pg_depend\" does not exist"),
                    ));
                }
            }
            let types = ["oid", "oid", "int4", "oid", "oid", "int4", "char"];
            let targets = DEPEND_COLS
                .iter()
                .zip(types)
                .map(|(c, ty)| {
                    let v = if names.iter().any(|n| n == c) {
                        qcol("x", c)
                    } else {
                        mk(NodeEnum::AConst(pb::AConst {
                            isnull: true,
                            val: None,
                            location: -1,
                        }))
                    };
                    target(cast(v, ty))
                })
                .collect();
            let sub = mk(NodeEnum::RangeSubselect(Box::new(pb::RangeSubselect {
                lateral: false,
                subquery: s.select_stmt.clone(),
                alias: Some(pb::Alias {
                    aliasname: "x".into(),
                    colnames: names
                        .iter()
                        .map(|n| mk(NodeEnum::String(pb::String { sval: n.clone() })))
                        .collect(),
                }),
            })));
            let (_, rows) = run(pb::SelectStmt {
                target_list: targets,
                from_clause: vec![sub],
                ..Default::default()
            })?;
            for r in &rows {
                if let Some(i) = r.iter().position(Value::is_null) {
                    return Err(Error::new(
                        code::NOT_NULL_VIOLATION,
                        format!(
                            "null value in column \"{}\" of relation \"pg_depend\" violates not-null constraint",
                            DEPEND_COLS[i]
                        ),
                    ));
                }
                if let Some(row) = depend_row(r) {
                    d.cat.depend_edits.add(row);
                }
            }
            d.save()?;
            Ok((Vec::new(), Vec::new(), format!("INSERT 0 {}", rows.len())))
        }
        _ => Err(crate::error::internal("not a pg_depend statement")),
    }
}

/// The pg_class statistics columns a direct catalog change may set, with their types.
fn class_stat_type(col: &str) -> Option<&'static str> {
    Some(match col {
        "reltuples" => "float4",
        "relpages" | "relallvisible" | "relallfrozen" => "int4",
        _ => return None,
    })
}

/// Whether `s` could be a MERGE into pg_class (or a plain view of it) that only updates
/// its statistics columns: every WHEN is MATCHED, with UPDATE SET of those or DO NOTHING.
pub(crate) fn might_merge_pg_class(s: &pb::MergeStmt) -> bool {
    s.with_clause.is_none()
        && !s.merge_when_clauses.is_empty()
        && s.merge_when_clauses.iter().all(|w| {
            let Some(NodeEnum::MergeWhenClause(w)) = inner(w) else {
                return false;
            };
            w.match_kind == pb::MergeMatchKind::MergeWhenMatched as i32
                && match pb::CmdType::try_from(w.command_type) {
                    Ok(pb::CmdType::CmdNothing) => true,
                    Ok(pb::CmdType::CmdUpdate) => w.target_list.iter().all(|t| {
                        matches!(inner(t), Some(NodeEnum::ResTarget(rt))
                            if rt.indirection.is_empty() && class_stat_type(&rt.name).is_some())
                    }),
                    _ => false,
                }
        })
}

/// Whether `rv` names pg_class itself, or a view that is a plain `SELECT * FROM pg_class`
/// (automatically updatable, so a MERGE into it changes pg_class).
fn names_pg_class(ctx: &crate::exec::ExecCtx, rv: &pb::RangeVar) -> bool {
    let is_class = |rv: &pb::RangeVar| {
        rv.relname == "pg_class" && (rv.schemaname.is_empty() || rv.schemaname == "pg_catalog")
    };
    let cat = ctx.catalog();
    let schema = (!rv.schemaname.is_empty()).then_some(rv.schemaname.as_str());
    match cat.find(schema, &rv.relname, &ctx.search_path) {
        Some(crate::catalog::Relation::View(v)) if !v.materialized => {
            let Ok(stmts) = crate::sql::parse::parse(&v.query) else {
                return false;
            };
            let [stmt] = stmts.as_slice() else {
                return false;
            };
            let Some(NodeEnum::SelectStmt(sel)) = stmt.stmt.as_deref().and_then(inner) else {
                return false;
            };
            // `*`, or pg_class's columns each as itself (as a stored view lists them).
            let class_cols = crate::catalog::system::SysTable::PgClass.columns();
            let plain = |t: &pb::Node, want: Option<&str>| {
                let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                    return false;
                };
                let Some(NodeEnum::ColumnRef(c)) = rt.val.as_deref().and_then(inner) else {
                    return false;
                };
                match (c.fields.last().and_then(inner), want) {
                    (Some(NodeEnum::AStar(_)), None) => c.fields.len() == 1,
                    (Some(NodeEnum::String(f)), Some(w)) => {
                        f.sval == w && (rt.name.is_empty() || rt.name == w)
                    }
                    _ => false,
                }
            };
            let star = match sel.target_list.as_slice() {
                [t] if plain(t, None) => true,
                ts => {
                    ts.len() == class_cols.len()
                        && ts.iter().zip(class_cols.iter()).all(|(t, (n, _))| plain(t, Some(n)))
                }
            };
            star && sel.where_clause.is_none()
                && sel.group_clause.is_empty()
                && sel.having_clause.is_none()
                && sel.distinct_clause.is_empty()
                && sel.limit_count.is_none()
                && sel.limit_offset.is_none()
                && matches!(sel.from_clause.as_slice(), [f] if matches!(inner(f), Some(NodeEnum::RangeVar(r)) if is_class(r)))
        }
        Some(_) => false,
        None => is_class(rv),
    }
}

/// MERGE into pg_class (or a plain view of it) by a superuser, its WHEN MATCHED clauses
/// updating relations' statistics columns: the matched rows are found by querying, the
/// first clause whose condition holds acts on each, and the new values go into the
/// relations' stored statistics (as `update_pg_class`). None: not such a MERGE.
pub(crate) fn merge_pg_class(
    ctx: &crate::exec::ExecCtx,
    s: &pb::MergeStmt,
) -> Result<Option<DependDml>> {
    let Some(rv) = s.relation.as_ref() else {
        return Ok(None);
    };
    if !might_merge_pg_class(s) || !names_pg_class(ctx, rv) {
        return Ok(None);
    }
    let cat = ctx.catalog();
    let me = ctx.sess.current_role(&cat);
    if !cat.is_superuser(me) {
        return Err(Error::new(
            code::INSUFFICIENT_PRIVILEGE,
            "permission denied for table pg_class",
        ));
    }
    let refname = rv
        .alias
        .as_ref()
        .map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
    // oid and the statistics now, then per clause its condition and its SET values, then
    // RETURNING.
    let mut targets: Vec<pb::Node> = ["oid", "relpages", "reltuples", "relallvisible"]
        .iter()
        .map(|c| target(qcol(&refname, c)))
        .collect();
    let mut clauses: Vec<(bool, Vec<String>)> = Vec::new();
    for w in &s.merge_when_clauses {
        let Some(NodeEnum::MergeWhenClause(w)) = inner(w) else {
            continue;
        };
        let cond = w.condition.as_deref().cloned().unwrap_or_else(|| {
            mk(NodeEnum::AConst(pb::AConst {
                isnull: false,
                val: Some(pb::a_const::Val::Boolval(pb::Boolean { boolval: true })),
                location: -1,
            }))
        });
        targets.push(target(cast(cond, "bool")));
        let update = w.command_type == pb::CmdType::CmdUpdate as i32;
        let mut cols = Vec::new();
        for t in &w.target_list {
            let Some(NodeEnum::ResTarget(rt)) = inner(t) else {
                continue;
            };
            let ty = class_stat_type(&rt.name).unwrap_or("float4");
            let val = rt.val.as_deref().cloned().unwrap_or_else(|| {
                mk(NodeEnum::AConst(pb::AConst {
                    isnull: true,
                    val: None,
                    location: -1,
                }))
            });
            targets.push(target(cast(val, ty)));
            cols.push(rt.name.clone());
        }
        clauses.push((update, cols));
    }
    let nfixed = targets.len();
    targets.extend(crate::sql::binder::ast::returning(&s.returning_clause).iter().cloned());
    let mut from = vec![mk(NodeEnum::RangeVar(rv.clone()))];
    from.extend(s.source_relation.as_deref().cloned());
    let sel = pb::SelectStmt {
        target_list: targets,
        from_clause: from,
        where_clause: s.join_condition.clone(),
        ..Default::default()
    };
    let child = ctx.child(&[]);
    let (cols, rows) = {
        let node = mk(NodeEnum::SelectStmt(Box::new(sel)));
        let mut b = Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&node)?;
        let rows = crate::exec::collect(&bq.plan, &child, None)?;
        (bq.cols, rows)
    };
    let num = |v: &Value| match v {
        Value::Int4(i) => *i as f64,
        Value::Float4(f) => *f as f64,
        other => other.as_f64().unwrap_or(0.0),
    };
    let mut d = Ddl::new(ctx, "");
    let mut done: Vec<u32> = Vec::new();
    let mut out = Vec::new();
    for r in &rows {
        let Value::Oid(oid) = r[0] else { continue };
        let mut at = 4;
        let mut acted = None;
        for (update, set_cols) in &clauses {
            let hit = r[at] == Value::Bool(true);
            let vals = &r[at + 1..at + 1 + set_cols.len()];
            at += 1 + set_cols.len();
            if hit {
                acted = Some((*update, set_cols, vals));
                break;
            }
        }
        let Some((update, set_cols, vals)) = acted else {
            continue;
        };
        if done.contains(&oid) {
            return Err(Error::new(
                code::CARDINALITY_VIOLATION,
                "MERGE command cannot affect row a second time",
            )
            .hint("Ensure that not more than one source row matches any one target row."));
        }
        done.push(oid);
        if !update {
            continue;
        }
        let mut st = RelStats {
            pages: num(&r[1]) as i32,
            tuples: num(&r[2]),
            allvisible: num(&r[3]) as i32,
            allfrozen: d.cat.relstats.get(&oid).map_or(0, |s| s.allfrozen),
        };
        for (name, v) in set_cols.iter().zip(vals) {
            if v.is_null() {
                return Err(Error::new(
                    code::NOT_NULL_VIOLATION,
                    format!(
                        "null value in column \"{name}\" of relation \"pg_class\" violates not-null constraint"
                    ),
                ));
            }
            match name.as_str() {
                "reltuples" => st.tuples = num(v),
                "relpages" => st.pages = num(v) as i32,
                "relallvisible" => st.allvisible = num(v) as i32,
                "relallfrozen" => st.allfrozen = num(v) as i32,
                _ => {}
            }
        }
        if oid >= crate::catalog::FIRST_USER_OID {
            std::sync::Arc::make_mut(&mut d.cat.relstats).insert(oid, st);
        }
        out.push(r[nfixed..].to_vec());
    }
    d.save()?;
    let n = out.len();
    Ok(Some((cols[nfixed..].to_vec(), out, format!("MERGE {n}"))))
}
