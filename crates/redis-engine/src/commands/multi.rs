//! Transactions: MULTI, EXEC, DISCARD, WATCH, UNWATCH.

use super::*;
use crate::client::Multi;
use crate::cmd;
use crate::engine::{Effect, Mode};

pub static COMMANDS: &[Command] = &[
    cmd!("multi", 1, NOSCRIPT | LOADING | STALE | FAST | ALLOW_BUSY | TXN_CONTROL, 0, 0, 0, multi, Transactions),
    cmd!("exec", 1, NOSCRIPT | LOADING | STALE | SKIP_SLOWLOG | TXN_CONTROL, 0, 0, 0, exec, Transactions),
    cmd!("discard", 1, NOSCRIPT | LOADING | STALE | FAST | ALLOW_BUSY | TXN_CONTROL, 0, 0, 0, discard, Transactions),
    cmd!("watch", -2, NOSCRIPT | LOADING | STALE | FAST | ALLOW_BUSY | TXN_CONTROL, 1, -1, 1, watch, Transactions),
    cmd!("unwatch", 1, NOSCRIPT | LOADING | STALE | FAST | ALLOW_BUSY, 0, 0, 0, unwatch, Transactions),
];

pub fn multi(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.client.multi.is_some() {
        return Err(Error::new("ERR MULTI calls can not be nested"));
    }
    cx.client.multi = Some(Multi::default());
    Ok(Reply::Ok)
}

pub fn discard(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.client.multi.take().is_none() {
        return Err(Error::new("ERR DISCARD without MULTI"));
    }
    let id = cx.client.id;
    cx.eng.unwatch_all(id);
    Ok(Reply::Ok)
}

pub fn watch(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    if cx.client.multi.is_some() {
        return Err(Error::new("ERR WATCH inside MULTI is not allowed"));
    }
    let now = cx.now;
    for k in &args[1..] {
        cx.eng.watch(cx.client, k, now);
    }
    Ok(Reply::Ok)
}

pub fn unwatch(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let id = cx.client.id;
    cx.eng.unwatch_all(id);
    Ok(Reply::Ok)
}

pub fn exec(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let Some(m) = cx.client.multi.take() else {
        return Err(Error::new("ERR EXEC without MULTI"));
    };
    let id = cx.client.id;
    cx.no_propagate();
    if m.error {
        cx.eng.unwatch_all(id);
        return Err(Error::new("EXECABORT Transaction discarded because of previous errors."));
    }
    let failed = cx.eng.watch_failed(id, cx.now);
    cx.eng.unwatch_all(id);
    if failed {
        return Ok(Reply::NilArray);
    }
    let start = cx.eng.effects.len();
    let mut replies = Vec::with_capacity(m.queued.len());
    cx.eng.exec_out_bytes = format!("*{}\r\n", m.queued.len()).len() as u64;
    for args in &m.queued {
        replies.push(cx.call(args, Mode::Exec));
    }
    wrap_effects(cx, start);
    Ok(Reply::Array(replies))
}

/// Wrap the effects produced since `start` in MULTI/EXEC so replicas apply them atomically.
pub fn wrap_effects(cx: &mut Cx, start: usize) {
    let n = cx.eng.effects.len() - start;
    if n > 1 {
        let db = cx.eng.effects[start].db;
        cx.eng.effects.insert(start, Effect { db, args: vec![Bytes::from_static(b"MULTI")] });
        cx.eng.effects.push(Effect { db, args: vec![Bytes::from_static(b"EXEC")] });
    }
}
