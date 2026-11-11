//! tcn (`contrib/tcn`): `triggered_change_notification`, an AFTER ROW trigger that NOTIFYs
//! the change of a row by its primary key: `"table",I|U|D,"col"='value',...` on the channel
//! the trigger names (`tcn` by default).

use super::native::{Native, TriggerData};
use crate::Result;
use crate::error::{Error, code};
use crate::exec::{ExecCtx, Row};

pub(super) fn symbol(sym: &str) -> Option<Native> {
    match sym {
        "triggered_change_notification" => Some(Native::scalar(|_, _| {
            Err(Error::new(
                code::TRIGGER_PROTOCOL_VIOLATED,
                "triggered_change_notification: must be called as trigger",
            ))
        })),
        _ => None,
    }
}

pub(super) fn trigger(sym: &str) -> Option<super::native::TriggerImpl> {
    match sym {
        "triggered_change_notification" => Some(notify),
        _ => None,
    }
}

/// strcpy_quoted: `s` in `q`, a `q` inside doubled.
fn quoted(out: &mut String, s: &str, q: char) {
    out.push(q);
    for c in s.chars() {
        if c == q {
            out.push(q);
        }
        out.push(c);
    }
    out.push(q);
}

fn violated(m: &str) -> Error {
    Error::new(code::TRIGGER_PROTOCOL_VIOLATED, format!("triggered_change_notification: {m}"))
}

fn notify(t: TriggerData, ctx: &ExecCtx) -> Result<Option<Row>> {
    if t.before {
        return Err(violated("must be called after the change"));
    }
    if !t.row {
        return Err(violated("must be called for each row"));
    }
    let operation = if t.update {
        'U'
    } else if t.delete {
        'D'
    } else {
        'I'
    };
    if t.args.len() > 1 {
        return Err(violated("must not be called with more than one parameter"));
    }
    let channel = t.args.first().map_or("tcn", String::as_str);
    // (tg_trigtuple: the inserted row, else the old one.)
    let row = if operation == 'I' { t.new.as_ref() } else { t.old.as_ref() };
    let cat = ctx.catalog();
    let mut indexes: Vec<_> = cat.indexes.values().filter(|i| i.table == t.table.oid).collect();
    indexes.sort_by_key(|i| i.oid);
    let Some(pk) = indexes.into_iter().find(|i| i.primary && i.valid) else {
        return Err(violated("must be called on a table with a primary key"));
    };
    let mut payload = String::new();
    quoted(&mut payload, &t.table.name, '"');
    payload.push(',');
    payload.push(operation);
    for c in &pk.columns {
        let crate::catalog::IndexKey::Column(k) = c.key else { continue };
        let col = &t.table.columns[k];
        payload.push(',');
        quoted(&mut payload, &col.name, '"');
        payload.push('=');
        let v = row.and_then(|r| r.get(k)).filter(|v| !v.is_null());
        let text = v.map(|v| crate::types::text::format_value(v, &col.ty, ctx.fmt())).unwrap_or_default();
        quoted(&mut payload, &text, '\'');
    }
    crate::notify::check(channel, &payload)?;
    ctx.txn.borrow_mut().notify(channel, &payload);
    Ok(None)
}
