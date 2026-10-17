//! SAVE, BGSAVE and BGREWRITEAOF. Durability is the platform's log, so these only keep Redis'
//! bookkeeping: a background "child" runs from the command until the next cron tick, like a
//! fork that finishes before serverCron notices, with the same replies and INFO fields.

use super::*;
use crate::engine::{Engine, Mode};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildKind {
    Rdb,
    Aof,
}

#[derive(Debug)]
pub struct Child {
    pub kind: ChildKind,
    /// Unix seconds.
    pub start: i64,
    pub dirty_before: u64,
    pub keys: u64,
    /// When the work is done (ms): `rdb-key-save-delay` per key.
    pub end_ms: i64,
    /// Killed by FLUSHALL: reaped without counting as a save.
    pub killed: bool,
}

#[derive(Debug)]
pub struct Persist {
    pub child: Option<Child>,
    pub bgsave_scheduled: bool,
    pub aof_rewrite_scheduled: bool,
    pub rdb_last_time: i64,
    pub aof_last_time: i64,
    pub aof_rewrites: u64,
    pub total_forks: u64,
    pub latest_fork_us: u64,
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            child: None,
            bgsave_scheduled: false,
            aof_rewrite_scheduled: false,
            rdb_last_time: -1,
            aof_last_time: -1,
            aof_rewrites: 0,
            total_forks: 0,
            latest_fork_us: 0,
        }
    }
}

impl Persist {
    pub fn in_progress(&self, kind: ChildKind) -> bool {
        self.child.as_ref().is_some_and(|c| c.kind == kind)
    }

    /// FLUSHALL kills a background save: it's reaped at the next cron without effect.
    pub fn kill_rdb(&mut self, now_ms: i64) {
        if let Some(c) = self.child.as_mut().filter(|c| c.kind == ChildKind::Rdb) {
            c.killed = true;
            c.end_ms = now_ms;
        }
    }

    /// Seconds the running child of `kind` has taken so far, or -1.
    pub fn current_time(&self, kind: ChildKind, now_s: i64) -> i64 {
        self.child.as_ref().filter(|c| c.kind == kind).map_or(-1, |c| now_s - c.start)
    }
}

impl Engine {
    fn fork(&mut self, kind: ChildKind, now_ms: i64) {
        let started = std::time::Instant::now();
        let keys: u64 = self.dbs.iter().map(|d| d.len() as u64).sum();
        let end_ms = now_ms + (keys as i64).saturating_mul(self.cfg.rdb_key_save_delay.max(0)) / 1000;
        let dirty_before = self.dirty();
        let p = &mut self.persist;
        if kind == ChildKind::Rdb {
            self.stats.rdb_saves += 1;
        } else {
            p.aof_rewrites += 1;
        }
        p.child = Some(Child { kind, start: now_ms / 1000, dirty_before, keys, end_ms, killed: false });
        p.total_forks += 1;
        p.latest_fork_us = started.elapsed().as_micros().max(1) as u64;
    }

    /// serverCron's part: finish the running child, apply the `save` rules, then start what was
    /// scheduled.
    pub fn persist_cron(&mut self, now_ms: i64) {
        let now_s = now_ms / 1000;
        if self.persist.child.is_none() && self.persist.aof_rewrite_scheduled {
            self.persist.aof_rewrite_scheduled = false;
            self.fork(ChildKind::Aof, now_ms);
            return;
        }
        if let Some(c) = &self.persist.child {
            if c.end_ms <= now_ms
                && let Some(c) = self.persist.child.take()
            {
                match c.kind {
                    _ if c.killed => {}
                    ChildKind::Rdb => {
                        self.dirty_mark += c.dirty_before.min(self.dirty());
                        self.lastsave = now_s;
                        self.persist.rdb_last_time = now_s - c.start;
                    }
                    ChildKind::Aof => self.persist.aof_last_time = now_s - c.start,
                }
            }
        } else if self.cfg.save.iter().any(|&(secs, changes)| self.dirty() >= changes as u64 && now_s - self.lastsave > secs) {
            self.fork(ChildKind::Rdb, now_ms);
        }
        if self.persist.child.is_none() && self.persist.bgsave_scheduled {
            self.persist.bgsave_scheduled = false;
            self.fork(ChildKind::Rdb, now_ms);
        }
    }

    /// CONFIG SET appendonly: `startAppendOnly` (a rewrite now, or scheduled behind another child
    /// or a transaction) and `stopAppendOnly` (the rewrite is abandoned).
    pub fn set_appendonly(&mut self, on: bool, now_ms: i64, in_exec: bool) {
        let p = &mut self.persist;
        if !on {
            p.aof_rewrite_scheduled = false;
            if p.in_progress(ChildKind::Aof) {
                p.child = None;
            }
            return;
        }
        if p.child.as_ref().is_some_and(|c| c.kind != ChildKind::Aof) || in_exec {
            p.aof_rewrite_scheduled = true;
        } else {
            p.child = None;
            self.fork(ChildKind::Aof, now_ms);
        }
    }
}

/// SAVE
pub fn save(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.eng.persist.in_progress(ChildKind::Rdb) {
        return Err(Error::new("ERR Background save already in progress"));
    }
    cx.eng.stats.rdb_saves += 1;
    cx.eng.reset_dirty();
    cx.eng.lastsave = cx.now / 1000;
    Ok(Reply::Ok)
}

/// BGSAVE [SCHEDULE]
pub fn bgsave(cx: &mut Cx, args: &[Bytes]) -> CmdResult {
    let schedule = match args.len() {
        1 => false,
        2 if eq(&args[1], "schedule") => true,
        _ => return Err(SYNTAX),
    };
    let exec = cx.mode == Mode::Exec;
    let p = &mut cx.eng.persist;
    if p.in_progress(ChildKind::Rdb) {
        Err(Error::new("ERR Background save already in progress"))
    } else if p.child.is_some() || exec {
        if schedule || exec {
            p.bgsave_scheduled = true;
            Ok(Reply::status("Background saving scheduled"))
        } else {
            Err(Error::new(
                "ERR Another child process is active (AOF?): can't BGSAVE right now. Use BGSAVE SCHEDULE in order to schedule a BGSAVE whenever possible.",
            ))
        }
    } else {
        cx.eng.fork(ChildKind::Rdb, cx.now);
        Ok(Reply::status("Background saving started"))
    }
}

/// BGREWRITEAOF
pub fn bgrewriteaof(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let exec = cx.mode == Mode::Exec;
    let p = &mut cx.eng.persist;
    if p.in_progress(ChildKind::Aof) {
        Err(Error::new("ERR Background append only file rewriting already in progress"))
    } else if p.child.is_some() || exec {
        p.aof_rewrite_scheduled = true;
        Ok(Reply::status("Background append only file rewriting scheduled"))
    } else {
        cx.eng.fork(ChildKind::Aof, cx.now);
        Ok(Reply::status("Background append only file rewriting started"))
    }
}
