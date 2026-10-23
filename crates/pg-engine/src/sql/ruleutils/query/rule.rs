//! `pg_get_ruledef` of a CREATE RULE rule (`make_ruledef`): its event, WHERE condition and
//! actions, printed over OLD and NEW (a level enclosing the actions, whose columns show as
//! `old.x` / `new.x`).

use super::super::{NO_INDENT, RTABLES};
use super::{Nest, Nested, Views, keyword};
use crate::catalog::{Rule, quote_ident};
use crate::exec::ExecCtx;
use crate::sql::binder::rules::{RuleSource, expand_old_new_star};
use crate::sql::binder::{Binder, Clause, Level, Scope, ast};
use crate::sql::plan::Plan;
use pg_query::NodeEnum;
use std::sync::Arc;

/// The rule's definition; `pretty`: pg_get_ruledef(oid, true) (fewer parentheses, the
/// relation named as the search path shows it).
pub(in crate::sql::ruleutils) fn rule_def(ctx: &ExecCtx, r: &Rule, pretty: bool) -> Option<String> {
    // Names resolve on the session's search path (pg_rules runs over pg_catalog's).
    let child = ctx.refreshed();
    let ctx = &child;
    let cat = ctx.catalog();
    let (table, schema) = match (cat.tables.get(&r.table), cat.views.get(&r.table)) {
        (Some(t), _) => (t.clone(), t.schema),
        (None, Some(v)) => (v.as_table(), v.schema),
        _ => return None,
    };
    let rel = if pretty {
        cat.qualified_name(schema, &table.name, &ctx.search_path)
    } else {
        format!(
            "{}.{}",
            quote_ident(cat.schema_name(schema)),
            quote_ident(&table.name)
        )
    };
    let event = match r.event {
        '1' => "SELECT",
        '2' => "UPDATE",
        '3' => "INSERT",
        _ => "DELETE",
    };
    let mut out = format!(
        "CREATE RULE {} AS\n    ON {event} TO {rel}",
        quote_ident(&r.name)
    );
    let n = table.columns.len();
    let src = RuleSource {
        plan: Arc::new(Plan::Empty {
            ncols: 2 * n,
            cols: None,
            info: None,
        }),
        table: table.clone(),
        old: Some(0),
        new: Some(n),
    };
    let names: Vec<String> = ["old", "new"]
        .iter()
        .flat_map(|w| {
            table
                .columns
                .iter()
                .map(move |c| format!("{w}.{}", quote_ident(&c.name)))
        })
        .collect();
    let mut b = Binder::new(ctx, vec![]);
    b.deparse = true;
    b.level().barrier = true;
    if let Some(q) = &r.qual {
        let node = ast::parse_expr(q).ok()?;
        let rels = src
            .rels(false)
            .into_iter()
            .filter(|x| match r.event {
                '3' => x.alias == "new",
                '4' => x.alias == "old",
                _ => true,
            })
            .collect();
        let mut lvl = Level::new();
        lvl.barrier = true;
        lvl.scope = Scope { rels };
        b.levels.push(lvl);
        let e = b
            .in_clause(Clause::Where, |b| {
                let e = b.bind_expr(&node)?;
                b.coerce_bool(e, "WHERE")
            })
            .ok();
        b.levels.pop();
        out.push_str("\n   WHERE ");
        out.push_str(&super::super::deparse(&e?, &names, ctx, pretty)?);
    }
    out.push_str(" DO ");
    if r.instead {
        out.push_str("INSTEAD ");
    }
    if r.actions.is_empty() {
        out.push_str("NOTHING;");
        return Some(out);
    }
    // The actions: a level whose OLD / NEW the action level below reads.
    let mut lvl = Level::new();
    lvl.barrier = true;
    lvl.scope = Scope {
        rels: src.rels(true),
    };
    b.levels.push(lvl);
    b.levels.push(Level::new());
    let mut v = Views {
        b: &mut b,
        ctx,
        pretty,
        wrap: 0,
        used: Vec::new(),
        enclosing: vec![Vec::new(), names],
    };
    let mut texts = Vec::new();
    for a in &r.actions {
        let mut node = ast::parse_stmt(a).ok()?;
        expand_old_new_star(&mut node, &table);
        let mut buf = String::new();
        v.used.clear();
        match node.node.as_ref()? {
            NodeEnum::InsertStmt(_) | NodeEnum::UpdateStmt(_) | NodeEnum::DeleteStmt(_) => {
                v.dml(&node, &mut buf, 0)?
            }
            NodeEnum::SelectStmt(_) => {
                let _g = crate::scoped::set(&RTABLES, RTABLES.get() + 1, "ruleutils::RTABLES.rule");
                v.query(&node, &mut buf, 0, &[], Nest::Action)?
            }
            NodeEnum::NotifyStmt(s) => {
                let mut indent = 0;
                if !NO_INDENT.get() {
                    keyword(&mut buf, &mut indent, "", 0, super::STD, 1);
                }
                buf.push_str(&format!("NOTIFY {}", quote_ident(&s.conditionname)));
                if !s.payload.is_empty() {
                    buf.push_str(&format!(", '{}'", s.payload.replace('\'', "''")));
                }
            }
            _ => return None,
        }
        texts.push(buf);
    }
    // Appended as Postgres appends to one buffer: a keyword starting a new line trims the
    // spaces before it.
    let append = |out: &mut String, t: &str| {
        if t.starts_with('\n') {
            while out.ends_with(' ') {
                out.pop();
            }
        }
        out.push_str(t);
    };
    if texts.len() > 1 {
        out.push('(');
        for t in texts {
            append(&mut out, &t);
            // (pg_get_ruledef always indents: PRETTYFLAG_INDENT.)
            out.push_str(";\n");
        }
        out.push_str(");");
    } else {
        append(&mut out, &texts[0]);
        out.push(';');
    }
    Some(out)
}
