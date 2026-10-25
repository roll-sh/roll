//! lo (`contrib/lo/lo.c`): the `lo` domain's trigger function `lo_manage`, which unlinks a
//! large object when the row referring to it changes it or goes.

use super::native::{Native, TriggerData};
use crate::Result;
use crate::exec::{ExecCtx, Row};
use crate::types::Value;

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "lo_manage" => Some(Native::scalar(|_, _| {
            Err(crate::error::internal("lo_manage: not fired by trigger manager"))
        })),
        _ => None,
    }
}

pub(super) fn trigger(sym: &str) -> Option<super::native::TriggerImpl> {
    match sym {
        "lo_manage" => Some(lo_manage),
        _ => None,
    }
}

/// `be_lo_unlink` of the large object a column's value names.
fn unlink(oid_text: &str, ctx: &ExecCtx) -> Result<()> {
    let oid: u32 = oid_text.trim().parse().unwrap_or(0);
    let def = crate::sql::func::resolve_function("lo_unlink", &[crate::types::DataType::Oid])?.def;
    let ret = crate::types::DataType::Int4;
    let fc = crate::sql::func::FnCtx {
        exec: ctx,
        ret: &ret,
        args: &[],
        coll: None,
    };
    (def.imp)(&[Value::Oid(oid)], &fc).map(|_| ())
}

fn lo_manage(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    if !t.row {
        return Err(crate::error::internal(format!("{}: must be fired for row", t.name)));
    }
    let Some(col) = t.args.first() else {
        return Err(crate::error::internal(format!(
            "{}: no column name provided in the trigger definition",
            t.name
        )));
    };
    let Some(attnum) = t.table.column_index(col) else {
        return Err(crate::error::internal(format!("{}: column \"{col}\" does not exist", t.name)));
    };
    let text = |r: &Option<Row>| -> Option<String> {
        let v = r.as_ref()?.get(attnum)?;
        (!v.is_null()).then(|| crate::types::text::format_value(v, &t.table.columns[attnum].ty, ctx.fmt()))
    };
    // An UPDATE assigning the column: the old object goes when it changes.
    if t.new.is_some() && t.updated_cols.contains(&attnum) {
        let (orig, newv) = (text(&t.old), text(&t.new));
        if let Some(o) = &orig
            && newv.as_ref() != Some(o)
        {
            unlink(o, ctx)?;
        }
    }
    if t.delete
        && let Some(o) = text(&t.old)
    {
        unlink(&o, ctx)?;
    }
    Ok(if t.update { t.new } else { t.old })
}
