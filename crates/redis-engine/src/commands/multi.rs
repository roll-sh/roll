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
    cx.client.executing = Some((m.queued.len() as i64, m.queued.iter().flatten().map(|a| a.len()).sum()));
    for args in &m.queued {
        replies.push(cx.call(args, Mode::Exec));
    }
    cx.client.executing = None;
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

#[cfg(test)]
mod tests {
    use crate::client::Client;
    use crate::{Engine, Outcome, Reply};
    use bytes::Bytes;

    fn run(eng: &mut Engine, c: &mut Client, line: &str) -> Reply {
        let args: Vec<Bytes> = line.split(' ').map(|s| Bytes::copy_from_slice(s.as_bytes())).collect();
        match eng.call(c, &args) {
            Outcome::Reply(r) => r,
            Outcome::Blocked => panic!("blocked"),
        }
    }

    /// A key stored already expired, watched, then removed: logically unchanged, EXEC runs
    /// (touchWatchedKey's `wk->expired`).
    #[test]
    fn watched_expired_key_deleted_later() {
        let mut eng = Engine::standalone();
        let mut c = Client::new(1);
        // With RediSearch subscribed to keyspace events, as the compat suites run.
        run(&mut eng, &mut c, "FT.CREATE sub ON HASH PREFIX 1 sub: SCHEMA t TEXT");
        run(&mut eng, &mut c, "FT.DROPINDEX sub");
        assert_eq!(run(&mut eng, &mut c, "MSETEX 1 w 1e3 EXAT 1"), Reply::Int(1));
        assert_eq!(run(&mut eng, &mut c, "WATCH w"), Reply::Ok);
        eng.active_expire(1_000_000);
        assert_eq!(run(&mut eng, &mut c, "MULTI"), Reply::Ok);
        assert_eq!(run(&mut eng, &mut c, "EXEC"), Reply::Array(vec![]));
        // Without the cycle in between too.
        assert_eq!(run(&mut eng, &mut c, "MSETEX 1 w 1e3 EXAT 1"), Reply::Int(1));
        assert_eq!(run(&mut eng, &mut c, "WATCH w"), Reply::Ok);
        assert_eq!(run(&mut eng, &mut c, "MULTI"), Reply::Ok);
        assert_eq!(run(&mut eng, &mut c, "EXEC"), Reply::Array(vec![]));
    }
}
