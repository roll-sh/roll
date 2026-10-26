//! UPDATE / INSERT INTO / DELETE FROM pg_description by a superuser (pgjdbc's metadata
//! tests move a function's comment onto a table). The rows a statement touches are found
//! by querying pg_description; each is then changed in the catalog's comments
//! (`Catalog::put_description` / `remove_description`), keeping its unique index
//! (objoid, classoid, objsubid) as Postgres's heap update does.

use super::Ddl;
use crate::error::{Error, Result, code};
use crate::sql::binder::Binder;
use crate::sql::binder::ast::{inner, mk, returning};
use crate::types::Value;
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

const COLS: [&str; 4] = ["objoid", "classoid", "objsubid", "description"];
const TYPES: [&str; 4] = ["oid", "oid", "int4", "text"];

fn names_pg_description(rv: &Option<pb::RangeVar>) -> bool {
    rv.as_ref().is_some_and(|rv| {
        rv.relname == "pg_description" && (rv.schemaname.is_empty() || rv.schemaname == "pg_catalog")
    })
}

/// Whether `node` is DML on pg_description this module runs.
pub(crate) fn is_pg_description_dml(node: &NodeEnum) -> bool {
    match node {
        NodeEnum::UpdateStmt(s) => {
            names_pg_description(&s.relation) && s.with_clause.is_none() && returning(&s.returning_clause).is_empty()
        }
        NodeEnum::DeleteStmt(s) => {
            names_pg_description(&s.relation) && s.with_clause.is_none() && returning(&s.returning_clause).is_empty()
        }
        NodeEnum::InsertStmt(s) => {
            names_pg_description(&s.relation)
                && s.with_clause.is_none()
                && s.on_conflict_clause.is_none()
                && s.select_stmt.is_some()
                && returning(&s.returning_clause).is_empty()
        }
        _ => false,
    }
}

fn s(v: &str) -> pb::Node {
    mk(NodeEnum::String(pb::String { sval: v.to_string() }))
}

fn colref(fields: &[&str]) -> pb::Node {
    mk(NodeEnum::ColumnRef(pb::ColumnRef { fields: fields.iter().map(|f| s(f)).collect(), location: -1 }))
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
            names: vec![s("pg_catalog"), s(ty)],
            typemod: -1,
            location: -1,
            ..Default::default()
        }),
        location: -1,
    })))
}

fn null() -> pb::Node {
    mk(NodeEnum::AConst(pb::AConst { isnull: true, val: None, location: -1 }))
}

fn undefined_column(n: &str) -> Error {
    Error::new(code::UNDEFINED_COLUMN, format!("column \"{n}\" of relation \"pg_description\" does not exist"))
}

/// A pg_description row out of four values; Err when one is NULL (its NOT NULL).
fn row(r: &[Value]) -> Result<(u32, u32, i32, String)> {
    if let Some(i) = r.iter().position(Value::is_null) {
        let shown: Vec<String> = r
            .iter()
            .map(|v| match v {
                Value::Null => "null".to_string(),
                Value::Text(t) => t.to_string(),
                Value::Oid(o) => o.to_string(),
                Value::Int4(n) => n.to_string(),
                other => crate::types::text::format_value(other, &crate::types::DataType::Text, &Default::default()),
            })
            .collect();
        return Err(Error::new(
            code::NOT_NULL_VIOLATION,
            format!("null value in column \"{}\" of relation \"pg_description\" violates not-null constraint", COLS[i]),
        )
        .detail(format!("Failing row contains ({}).", shown.join(", ")))
        .table("pg_catalog", "pg_description")
        .column(COLS[i]));
    }
    let oid = |v: &Value| match v {
        Value::Oid(o) => *o,
        other => other.as_f64().unwrap_or(0.0) as u32,
    };
    let sub = match &r[2] {
        Value::Int4(i) => *i,
        other => other.as_f64().unwrap_or(0.0) as i32,
    };
    let text = match &r[3] {
        Value::Text(t) => t.to_string(),
        other => crate::types::text::format_value(other, &crate::types::DataType::Text, &Default::default()),
    };
    Ok((oid(&r[0]), oid(&r[1]), sub, text))
}

/// The query statement `node` (`is_pg_description_dml`) runs to find its rows: an UPDATE's
/// old and new rows (eight columns), a DELETE's rows, an INSERT's new rows. Parse binds it to
/// type the statement's parameters.
pub(crate) fn description_query(node: &NodeEnum) -> Result<pb::Node> {
    let alias = |rv: &pb::RangeVar| rv.alias.as_ref().map_or_else(|| rv.relname.clone(), |a| a.aliasname.clone());
    let sel = match node {
        NodeEnum::UpdateStmt(st) => {
            let rv = st.relation.clone().unwrap();
            let rel = alias(&rv);
            let mut set: [Option<pb::Node>; 4] = Default::default();
            for t in &st.target_list {
                let Some(NodeEnum::ResTarget(rt)) = inner(t) else { continue };
                let i = COLS.iter().position(|c| *c == rt.name).ok_or_else(|| undefined_column(&rt.name))?;
                set[i] = Some(rt.val.as_deref().cloned().unwrap_or_else(null));
            }
            let mut targets: Vec<pb::Node> = COLS.iter().map(|c| target(colref(&[&rel, c]))).collect();
            for (i, v) in set.into_iter().enumerate() {
                let v = v.unwrap_or_else(|| colref(&[&rel, COLS[i]]));
                targets.push(target(cast(v, TYPES[i])));
            }
            let mut from = vec![mk(NodeEnum::RangeVar(rv))];
            from.extend(st.from_clause.iter().cloned());
            pb::SelectStmt { target_list: targets, from_clause: from, where_clause: st.where_clause.clone(), ..Default::default() }
        }
        NodeEnum::DeleteStmt(st) => {
            let rv = st.relation.clone().unwrap();
            let rel = alias(&rv);
            let mut from = vec![mk(NodeEnum::RangeVar(rv))];
            from.extend(st.using_clause.iter().cloned());
            pb::SelectStmt {
                target_list: COLS.iter().map(|c| target(colref(&[&rel, c]))).collect(),
                from_clause: from,
                where_clause: st.where_clause.clone(),
                ..Default::default()
            }
        }
        NodeEnum::InsertStmt(st) => {
            let names: Vec<String> = if st.cols.is_empty() {
                COLS.iter().map(|c| c.to_string()).collect()
            } else {
                st.cols
                    .iter()
                    .filter_map(|c| match inner(c) {
                        Some(NodeEnum::ResTarget(rt)) => Some(rt.name.clone()),
                        _ => None,
                    })
                    .collect()
            };
            if let Some(n) = names.iter().find(|n| !COLS.contains(&n.as_str())) {
                return Err(undefined_column(n));
            }
            let targets = COLS
                .iter()
                .zip(TYPES)
                .map(|(c, ty)| {
                    let v = if names.iter().any(|n| n == c) { colref(&["x", c]) } else { null() };
                    target(cast(v, ty))
                })
                .collect();
            let sub = mk(NodeEnum::RangeSubselect(Box::new(pb::RangeSubselect {
                lateral: false,
                subquery: st.select_stmt.clone(),
                alias: Some(pb::Alias { aliasname: "x".into(), colnames: names.iter().map(|n| s(n)).collect() }),
            })));
            pb::SelectStmt { target_list: targets, from_clause: vec![sub], ..Default::default() }
        }
        _ => return Err(crate::error::internal("not a pg_description statement")),
    };
    Ok(mk(NodeEnum::SelectStmt(Box::new(sel))))
}

/// Statement `node` (`is_pg_description_dml`) run: its command tag.
pub(crate) fn pg_description_dml(ctx: &crate::exec::ExecCtx, node: &NodeEnum) -> Result<String> {
    let cat = ctx.catalog();
    if !cat.is_superuser(ctx.sess.current_role(&cat)) {
        return Err(Error::new(code::INSUFFICIENT_PRIVILEGE, "permission denied for table pg_description"));
    }
    let query = description_query(node)?;
    let child = ctx.child(ctx.params);
    let rows = {
        let mut b = Binder::new(&child, Vec::new());
        let bq = b.bind_statement(&query)?;
        crate::exec::collect(&bq.plan, &child, None)?
    };
    let mut d = Ddl::new(ctx, "");
    let (tag, n) = match node {
        NodeEnum::UpdateStmt(_) => {
            for r in &rows {
                let (oo, oc, os, _) = row(&r[..4])?;
                let (no, nc, ns, text) = row(&r[4..])?;
                d.cat.remove_description(oo, oc, os);
                if d.cat.has_description(no, nc, ns) {
                    return Err(duplicate(no, nc, ns));
                }
                d.cat.put_description(no, nc, ns, text);
            }
            ("UPDATE", rows.len())
        }
        NodeEnum::DeleteStmt(_) => {
            // (A row USING joins more than once is deleted once.)
            let mut seen = Vec::new();
            for r in &rows {
                let (o, c, sub, _) = row(r)?;
                if !seen.contains(&(o, c, sub)) {
                    seen.push((o, c, sub));
                    d.cat.remove_description(o, c, sub);
                }
            }
            ("DELETE", seen.len())
        }
        _ => {
            for r in &rows {
                let (o, c, sub, text) = row(r)?;
                if d.cat.has_description(o, c, sub) {
                    return Err(duplicate(o, c, sub));
                }
                d.cat.put_description(o, c, sub, text);
            }
            ("INSERT 0", rows.len())
        }
    };
    d.save()?;
    Ok(format!("{tag} {n}"))
}

fn duplicate(o: u32, c: u32, sub: i32) -> Error {
    Error::new(code::UNIQUE_VIOLATION, "duplicate key value violates unique constraint \"pg_description_o_c_o_index\"")
        .detail(format!("Key (objoid, classoid, objsubid)=({o}, {c}, {sub}) already exists."))
        .table("pg_catalog", "pg_description")
        .constraint("pg_description_o_c_o_index")
}
