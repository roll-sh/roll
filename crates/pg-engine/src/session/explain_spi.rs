//! EXPLAIN run by a function (PL/pgSQL's EXECUTE, SPI): the query bound, run when ANALYZE
//! asks, and explained in the function's own execution context, giving the QUERY PLAN rows.

use crate::error::Result;
use crate::exec::{ExecCtx, Row, collect};
use crate::sql::binder::Binder;
use crate::sql::binder::ast::inner;
use crate::sql::plan::OutCol;
use crate::types::{DataType, Value};
use pg_query::NodeEnum;
use pg_query::protobuf as pb;

/// The rows and column of `EXPLAIN [options] query`, with `$n` of types `types`.
pub(crate) fn explain(
    ctx: &ExecCtx,
    s: &pb::ExplainStmt,
    types: Vec<DataType>,
) -> Result<(Vec<Row>, Vec<OutCol>)> {
    use crate::sql::explain::{Format, options};
    let o = options(&s.options)?;
    let Some(q) = s.query.as_deref() else {
        return Ok((Vec::new(), Vec::new()));
    };
    // CREATE TABLE AS / SELECT INTO: the query is explained, and with ANALYZE the table is
    // made by running the statement.
    let ctas = match inner(q) {
        Some(NodeEnum::SelectStmt(sel)) if crate::sql::binder::ast::select_into(sel).is_some() => {
            crate::sql::binder::ast::select_into_as_ctas(q).and_then(|n| n.node)
        }
        Some(NodeEnum::CreateTableAsStmt(c))
            if matches!(c.query.as_deref().and_then(inner), Some(NodeEnum::SelectStmt(_))) =>
        {
            Some(NodeEnum::CreateTableAsStmt(c.clone()))
        }
        _ => None,
    };
    // EXPLAIN EXECUTE: the SQL-prepared statement, its arguments bound and coerced to its
    // parameter types.
    let mut types = types;
    let mut values: Vec<Value> = Vec::new();
    let executed;
    let q = if let Some(NodeEnum::ExecuteStmt(e)) = inner(q) {
        let Some(p) = ctx.sess.prepared_sql.borrow().get(&e.name).cloned() else {
            return Err(crate::error::Error::new(
                crate::error::code::INVALID_SQL_STATEMENT_NAME,
                format!("prepared statement \"{}\" does not exist", e.name),
            ));
        };
        let mut b = Binder::new(ctx, Vec::new());
        for (n, t) in e.params.iter().zip(&p.param_types) {
            let x = b.bind_scalar(n, crate::sql::binder::Clause::Other)?;
            let x = b.coerce_assign(x, t, "parameter")?;
            values.push(crate::sql::expr::eval(&x, &[], ctx)?);
        }
        types = p.param_types.clone();
        executed = p
            .stmt
            .clone()
            .ok_or_else(|| crate::error::unsupported("EXPLAIN of this statement"))?;
        &executed
    } else {
        q
    };
    let query;
    let q = match &ctas {
        Some(NodeEnum::CreateTableAsStmt(c)) => {
            if crate::session::ddl::ctas_rel_exists(ctx, c)? {
                return Ok((Vec::new(), vec![OutCol::new("QUERY PLAN", DataType::Text)]));
            }
            query = c.query.as_deref().cloned().unwrap_or_default();
            &query
        }
        _ => q,
    };
    if !matches!(
        inner(q),
        Some(
            NodeEnum::SelectStmt(_)
                | NodeEnum::InsertStmt(_)
                | NodeEnum::UpdateStmt(_)
                | NodeEnum::DeleteStmt(_)
                | NodeEnum::MergeStmt(_)
        )
    ) {
        return Err(crate::error::unsupported("EXPLAIN of this statement"));
    }
    let generic = values.is_empty()
        || ctx
            .sess
            .settings
            .borrow()
            .get_raw("plan_cache_mode")
            .as_deref()
            == Some("force_generic_plan");
    let pctx = ctx.child(&values);
    let ctx = &pctx;
    let start = std::time::Instant::now();
    let bq = Binder::new(ctx, types).bind_statement(q)?;
    let planning = start.elapsed().as_secs_f64();
    bq.check_perms(ctx)?;
    let mut plan = (bq.plan.clone(), bq.hidden_rtes);
    let mut run = None;
    if o.analyze {
        let instr = std::rc::Rc::new(crate::exec::Instr::default());
        let saved = ctx.instr.borrow_mut().replace(instr.clone());
        let t = std::time::Instant::now();
        let r = match &ctas {
            Some(node) => crate::session::ddl::execute(ctx, node, "").map(|_| ()),
            None => collect(&bq.plan, ctx, None).map(|_| ()),
        };
        *ctx.instr.borrow_mut() = saved;
        r?;
        if let Some(ran) = ctx.explained.borrow_mut().take() {
            plan = ran;
        }
        run = Some((instr, t.elapsed().as_secs_f64()));
    }
    let shown: &[Value] = if generic { &[] } else { &values };
    let out = crate::sql::explain::explain(
        &plan.0,
        plan.1,
        &o,
        ctx,
        shown,
        run.as_ref().map(|(i, _)| &**i),
        planning,
        run.as_ref().map_or(0.0, |r| r.1),
    );
    let (ty, rows): (DataType, Vec<Row>) = match o.format {
        Format::Text => (
            DataType::Text,
            out.lines()
                .map(|l| vec![Value::Text(crate::sql::scs::session_literals(l).into())])
                .collect(),
        ),
        Format::Json => (DataType::Json, vec![vec![Value::Json(out.into())]]),
        Format::Xml => (DataType::Xml, vec![vec![Value::Text(out.into())]]),
        Format::Yaml => (DataType::Text, vec![vec![Value::Text(out.into())]]),
    };
    Ok((rows, vec![OutCol::new("QUERY PLAN", ty)]))
}
