//! SAVE, BGSAVE, BGREWRITEAOF and BACKUP. Durability is the platform's log, so these only keep
//! Redis' bookkeeping: a background "child" runs from the command until the next cron tick, like
//! a fork that finishes before serverCron notices, with the same replies and INFO fields. The AOF
//! state and the multi-part AOF manifest (file sequence numbers) are tracked so BACKUP names the
//! files Redis would.

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

/// `server.aof_state`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AofState {
    #[default]
    Off,
    WaitRewrite,
    On,
}

/// The AOF manifest's bookkeeping: file sequence numbers and the INCR files it lists.
#[derive(Debug, Default)]
pub struct AofManifest {
    pub base_seq: u64,
    pub incr_seq: u64,
    pub incrs: Vec<u64>,
    /// `appenddirname` holds files (a rewrite or an INCR wrote them).
    pub files: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackupState {
    #[default]
    Idle,
    Pending,
    Snapshotting,
    Incrementing,
    Sealed,
    Failed,
}

impl BackupState {
    fn name(self) -> &'static str {
        match self {
            BackupState::Idle => "idle",
            BackupState::Pending => "pending",
            BackupState::Snapshotting => "snapshotting",
            BackupState::Incrementing => "incrementing",
            BackupState::Sealed => "sealed",
            BackupState::Failed => "failed",
        }
    }
}

/// BACKUP's state: the window between START and SEAL, and the files it pinned.
#[derive(Debug, Default)]
pub struct Backup {
    pub state: BackupState,
    pub error: Option<String>,
    pub start: i64,
    pub end: i64,
    pub base: Option<String>,
    pub incr: Option<String>,
    pub manifest: Option<String>,
    /// The AOF directory belongs to the backup (it started the AOF on an empty one).
    pub owns_aof_dir: bool,
}

impl Backup {
    pub fn in_progress(&self) -> bool {
        matches!(self.state, BackupState::Pending | BackupState::Snapshotting | BackupState::Incrementing)
    }

    fn clear_files(&mut self) {
        (self.base, self.incr, self.manifest) = (None, None, None);
    }
}

#[derive(Debug)]
pub struct Persist {
    pub child: Option<Child>,
    pub aof: AofState,
    pub manifest: AofManifest,
    pub backup: Backup,
    pub bgsave_scheduled: bool,
    pub aof_rewrite_scheduled: bool,
    pub rdb_last_time: i64,
    pub aof_last_time: i64,
    pub aof_rewrites: u64,
    pub total_forks: u64,
    pub latest_fork_us: u64,
    /// Keys loaded and expired keys skipped by the last load (`rdb_last_load_keys_*`).
    pub last_load: (u64, u64),
}

impl Default for Persist {
    fn default() -> Self {
        Self {
            child: None,
            aof: AofState::Off,
            manifest: AofManifest::default(),
            backup: Backup::default(),
            bgsave_scheduled: false,
            aof_rewrite_scheduled: false,
            rdb_last_time: -1,
            aof_last_time: -1,
            aof_rewrites: 0,
            total_forks: 0,
            latest_fork_us: 0,
            last_load: (0, 0),
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
    /// The dataset as an RDB file, the bytes Redis' SAVE writes: aux fields, the function
    /// libraries, the hash templates, then every database with expiries and each key's idle time
    /// (LRU/LRM policies) or frequency counter (LFU policies).
    pub fn write_rdb(&self, out: &mut Vec<u8>, now: i64) -> std::io::Result<()> {
        use crate::config::Policy;
        use crate::rdb::{Lru, RdbWriter};
        let policy = self.cfg.maxmemory_policy;
        let lfu = policy.is_lfu();
        let idle = matches!(policy, Policy::AllKeysLru | Policy::VolatileLru | Policy::AllKeysLrm | Policy::VolatileLrm);
        let ctime = (now / 1000).to_string();
        let used = self.used_memory().to_string();
        let aux = [
            ("redis-ver", super::server::REDIS_VERSION),
            ("redis-bits", "64"),
            ("ctime", &ctime),
            ("used-mem", &used),
            ("aof-base", "0"),
        ];
        let mut w = RdbWriter::new(out, &aux, now)?;
        for code in self.scripting.library_codes() {
            w.function(&code)?;
        }
        w.templates(&self.hub.templates)?;
        for (i, db) in self.dbs.iter().enumerate() {
            if db.is_empty() {
                continue;
            }
            w.begin_db(i, db.len() as u64, db.expires() as u64)?;
            for (k, e) in db.keys.iter() {
                if e.expired(now) {
                    continue;
                }
                let lru = if lfu {
                    Lru::Freq(self.lfu_counter(e, now).min(255) as u8)
                } else if idle {
                    Lru::Idle(Engine::idle_secs(e, now))
                } else {
                    Lru::None
                };
                w.entry(k, e, lru)?;
            }
        }
        w.finish()?;
        Ok(())
    }

    /// `rdbLoad` into the current dataset: the file is parsed first (a bad one changes nothing),
    /// then its libraries are added and its keys inserted, tables sized by the file's RESIZEDB.
    /// A primary skips keys that have expired. A key or library that exists already is an error
    /// (after the libraries, part of the keys may be in), unless `merge` (`RDBFLAGS_ALLOW_DUP`):
    /// then the file's version replaces it.
    pub fn read_rdb(&mut self, data: &[u8], merge: bool, now: i64) -> Result<(), crate::rdb::RdbError> {
        use crate::rdb::{LoadItem, RdbError};
        let cfg = self.cfg.clone();
        let reg = self.hub.templates.clone();
        let (mut entries, mut functions, mut untemplate, mut sizes) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let keep_expired = self.replica;
        crate::rdb::load_with(data, &cfg, Some(&reg), |it| {
            match it {
                LoadItem::Function(code) => functions.push(code),
                LoadItem::Entry { db, key, entry } => entries.push((db, key, entry)),
                LoadItem::Untemplate { db, key } => untemplate.push((db, key)),
                LoadItem::ResizeDb { db, keys, expires } => sizes.push((db, keys, expires)),
                _ => {}
            }
            Ok::<(), RdbError>(())
        })?;
        let policy = if merge { super::scripting::RestorePolicy::Replace } else { super::scripting::RestorePolicy::Append };
        self.scripting.restore_libraries(&functions, policy).map_err(|_| RdbError::Corrupt("function library already exists"))?;
        for (i, keys, expires) in sizes {
            if let Some(d) = self.dbs.get_mut(i) {
                d.keys.expand(keys as usize);
                d.volatile.expand(expires as usize);
            }
        }
        let (mut loaded, mut expired) = (0, 0);
        for (i, key, entry) in entries {
            if !keep_expired && entry.expired(now) {
                expired += 1;
                continue;
            }
            let d = &mut self.dbs[i];
            if d.keys.contains_key(&key[..]) {
                if !merge {
                    return Err(RdbError::Corrupt("duplicate key"));
                }
                d.remove(&key);
            }
            d.insert_new(&key, entry);
            self.ts_loaded(i, &key);
            if self.sig.search_hook {
                crate::commands::ft::probe(self, &key, true);
                self.sig.search_events.push(("loaded", i as u16, crate::value::Str::from_slice(&key)));
                crate::commands::ft::drain(self);
            }
            loaded += 1;
        }
        let lim = crate::types::hash::Limits::of(&cfg);
        for (i, key) in untemplate {
            self.dbs[i].untemplate(&key, lim);
        }
        self.persist.last_load = (loaded, expired);
        Ok(())
    }

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

    /// `rewriteAppendOnlyFileBackground`: false when a child runs or a backup window is open.
    fn aof_rewrite(&mut self, now_ms: i64) -> bool {
        let p = &mut self.persist;
        if p.child.is_some() || matches!(p.backup.state, BackupState::Snapshotting | BackupState::Incrementing) {
            return false;
        }
        if p.aof == AofState::On {
            // `openNewIncrAofForAppend`: writes go to a new INCR file during the rewrite.
            p.manifest.incr_seq += 1;
            let seq = p.manifest.incr_seq;
            p.manifest.incrs.push(seq);
            p.manifest.files = true;
        }
        p.aof_rewrite_scheduled = false;
        self.fork(ChildKind::Aof, now_ms);
        true
    }

    /// `backgroundRewriteDoneHandler` on success: a new BASE, the rewritten INCRs become history.
    fn aof_rewrite_done(&mut self) {
        let p = &mut self.persist;
        let m = &mut p.manifest;
        m.base_seq += 1;
        m.files = true;
        if p.aof == AofState::WaitRewrite {
            m.incr_seq += 1;
            m.incrs.push(m.incr_seq);
        }
        // With the AOF open, the INCR being written stays; the others were rewritten.
        if p.aof == AofState::Off {
            m.incrs.clear();
        } else if let Some(&last) = m.incrs.last() {
            m.incrs = vec![last];
        }
        if p.aof == AofState::WaitRewrite {
            p.aof = AofState::On;
        }
        // `backupHandleSnapshotDone`: pin the new BASE.
        if p.backup.state == BackupState::Snapshotting {
            let base = self.base_file_name();
            let b = &mut self.persist.backup;
            b.base = Some(base);
            b.state = BackupState::Incrementing;
        }
    }

    fn base_file_name(&self) -> String {
        let suffix = if crate::config::aof_use_rdb_preamble(&self.cfg) { "rdb" } else { "aof" };
        format!("{}.{}.base.{suffix}", crate::config::appendfilename(&self.cfg), self.persist.manifest.base_seq)
    }

    fn incr_file_name(&self, seq: u64) -> String {
        format!("{}.{seq}.incr.aof", crate::config::appendfilename(&self.cfg))
    }

    /// `startAppendOnly`: a rewrite now (replacing a running one), or scheduled behind another
    /// child or a transaction.
    fn start_append_only(&mut self, now_ms: i64, in_exec: bool) -> bool {
        let p = &mut self.persist;
        p.aof = AofState::WaitRewrite;
        if p.child.as_ref().is_some_and(|c| c.kind != ChildKind::Aof) || in_exec {
            p.aof_rewrite_scheduled = true;
            return true;
        }
        if p.in_progress(ChildKind::Aof) {
            p.child = None;
        }
        if !self.aof_rewrite(now_ms) {
            self.persist.aof = AofState::Off;
            return false;
        }
        true
    }

    /// `stopAppendOnly`: the rewrite is abandoned, and a backup that needed the AOF fails.
    fn stop_append_only(&mut self) {
        let p = &mut self.persist;
        p.aof = AofState::Off;
        p.aof_rewrite_scheduled = false;
        if p.in_progress(ChildKind::Aof) {
            p.child = None;
        }
        self.backup_failed("appendonly is stopped");
    }

    /// Stop the AOF a backup started under `appendonly no`, keeping a postponed BGREWRITEAOF.
    fn backup_stop_temp_aof(&mut self) {
        if self.cfg.appendonly || self.persist.aof == AofState::Off {
            return;
        }
        let postponed = self.persist.aof_rewrite_scheduled;
        self.stop_append_only();
        self.persist.aof_rewrite_scheduled = postponed;
    }

    /// Forget the AOF files a backup created in a directory it owned.
    fn backup_remove_aof_dir(&mut self) {
        if self.persist.backup.owns_aof_dir {
            self.persist.manifest = AofManifest::default();
        }
    }

    /// `backupSetFailed`.
    pub(crate) fn backup_failed(&mut self, err: &str) {
        if !self.persist.backup.in_progress() {
            return;
        }
        let b = &mut self.persist.backup;
        b.state = BackupState::Failed;
        b.error = Some(err.to_string());
        self.backup_stop_temp_aof();
        self.backup_remove_aof_dir();
        let b = &mut self.persist.backup;
        b.clear_files();
        b.owns_aof_dir = false;
        b.end = 0;
    }

    /// `backupStartPendingSnapshot`: start (or join) the rewrite that makes the backup's BASE.
    fn backup_start_snapshot(&mut self, now_ms: i64) -> bool {
        if self.persist.backup.state != BackupState::Pending {
            return true;
        }
        if self.persist.in_progress(ChildKind::Aof) {
            // A rewrite feeding an open AOF can be reused; one without can't.
            if self.persist.aof != AofState::Off {
                self.persist.backup.start = now_ms / 1000;
                self.persist.backup.state = BackupState::Snapshotting;
            }
            return true;
        }
        if self.persist.child.is_some() {
            return true;
        }
        if self.persist.aof == AofState::Off {
            self.persist.backup.owns_aof_dir = !self.persist.manifest.files && !self.persist.aof_rewrite_scheduled;
            if !self.start_append_only(now_ms, false) {
                self.backup_failed("failed to temporarily enable AOF for backup");
                return false;
            }
        } else if !self.aof_rewrite(now_ms) {
            self.backup_failed("failed to start AOF rewrite for backup");
            return false;
        }
        if !self.persist.in_progress(ChildKind::Aof) {
            self.backup_failed("failed to start AOF rewrite child for backup");
            return false;
        }
        self.persist.backup.start = now_ms / 1000;
        self.persist.backup.state = BackupState::Snapshotting;
        true
    }

    /// `backupCron`: start a pending snapshot, expire a sealed backup after `backup-sealed-ttl`.
    fn backup_cron(&mut self, now_ms: i64) {
        self.backup_start_snapshot(now_ms);
        let now_s = now_ms / 1000;
        let ttl = crate::config::backup_sealed_ttl(&self.cfg);
        let b = &mut self.persist.backup;
        if b.state == BackupState::Sealed && ttl > 0 && b.end > 0 && now_s >= b.end && now_s - b.end >= ttl {
            *b = Backup::default();
        }
    }

    /// serverCron's part: the backup, a scheduled rewrite, finishing the running child, the
    /// `save` rules, then a scheduled BGSAVE.
    pub fn persist_cron(&mut self, now_ms: i64) {
        let now_s = now_ms / 1000;
        self.backup_cron(now_ms);
        if self.persist.child.is_none() && self.persist.aof_rewrite_scheduled && !self.persist.backup.in_progress() {
            self.aof_rewrite(now_ms);
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
                    ChildKind::Aof => {
                        self.persist.aof_last_time = now_s - c.start;
                        self.aof_rewrite_done();
                    }
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

    /// CONFIG SET appendonly (`updateAppendonly`): start or stop the AOF when its state differs
    /// (a backup may have opened one under `appendonly no`).
    pub fn set_appendonly(&mut self, on: bool, now_ms: i64, in_exec: bool) {
        if !on && self.persist.aof != AofState::Off {
            self.stop_append_only();
        } else if on && self.persist.aof == AofState::Off {
            self.start_append_only(now_ms, in_exec);
        }
    }

    /// The AOF at startup with `appendonly yes`: a BASE and an INCR are created.
    pub fn open_aof_at_start(&mut self) {
        let p = &mut self.persist;
        p.aof = AofState::On;
        p.manifest = AofManifest { base_seq: 1, incr_seq: 1, incrs: vec![1], files: true };
    }
}

/// SAVE
pub fn save(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.eng.persist.in_progress(ChildKind::Rdb) {
        return Err(Error::new("ERR Background save already in progress"));
    }
    for d in cx.eng.dbs.iter_mut() {
        d.keys.for_each_mut(|_, e| e.value.prepare_save());
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
    if p.backup.in_progress() {
        p.aof_rewrite_scheduled = true;
        Ok(Reply::status("Background append only file rewriting scheduled"))
    } else if p.in_progress(ChildKind::Aof) {
        Err(Error::new("ERR Background append only file rewriting already in progress"))
    } else if p.child.is_some() || exec {
        p.aof_rewrite_scheduled = true;
        Ok(Reply::status("Background append only file rewriting scheduled"))
    } else if cx.eng.aof_rewrite(cx.now) {
        Ok(Reply::status("Background append only file rewriting started"))
    } else {
        Err(Error::new("ERR Can't execute an AOF background rewriting. Please check the server logs for more information."))
    }
}

/// BACKUP START
pub fn backup_start(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let b = &cx.eng.persist.backup;
    if b.in_progress() {
        return Err(Error::new("ERR A backup is already in progress, ABORT it first"));
    }
    if b.state == BackupState::Sealed {
        return Err(Error::new("ERR A sealed backup exists, CLEANUP it first"));
    }
    cx.eng.persist.backup = Backup { state: BackupState::Pending, ..Backup::default() };
    // Not from inside EXEC: the BASE could capture half a transaction. The cron starts it.
    if cx.mode != Mode::Exec && !cx.eng.backup_start_snapshot(cx.now) {
        return Err(Error::new("ERR Can't start the AOF rewrite for the backup"));
    }
    Ok(Reply::Ok)
}

/// BACKUP SEAL: pin the INCR and the manifest; the backup is complete.
pub fn backup_seal(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.eng.persist.backup.state != BackupState::Incrementing {
        return Err(Error::new("ERR No backup ready to seal (must be in the incrementing state)"));
    }
    let m = &mut cx.eng.persist.manifest;
    if m.incrs.is_empty() {
        m.incr_seq += 1;
        m.incrs.push(m.incr_seq);
    }
    let incr = cx.eng.incr_file_name(*cx.eng.persist.manifest.incrs.last().expect("one INCR"));
    let manifest = format!("{}.manifest", crate::config::appendfilename(&cx.eng.cfg));
    let b = &mut cx.eng.persist.backup;
    (b.incr, b.manifest) = (Some(incr), Some(manifest));
    if !cx.eng.cfg.appendonly {
        cx.eng.persist.backup.state = BackupState::Sealed;
        cx.eng.backup_stop_temp_aof();
        cx.eng.backup_remove_aof_dir();
    } else {
        // The live AOF moves to a new INCR so the pinned one stops growing.
        let m = &mut cx.eng.persist.manifest;
        m.incr_seq += 1;
        m.incrs.push(m.incr_seq);
        cx.eng.persist.backup.state = BackupState::Sealed;
    }
    cx.eng.persist.backup.end = cx.now / 1000;
    Ok(Reply::Ok)
}

/// BACKUP ABORT
pub fn backup_abort(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if !cx.eng.persist.backup.in_progress() {
        return Err(Error::new("ERR No backup in progress"));
    }
    cx.eng.backup_failed("aborted by user");
    Ok(Reply::Ok)
}

/// BACKUP CLEANUP
pub fn backup_cleanup(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    if cx.eng.persist.backup.in_progress() {
        return Err(Error::new("ERR Backup is in progress"));
    }
    cx.eng.persist.backup = Backup::default();
    Ok(Reply::Ok)
}

/// BACKUP STATUS
pub fn backup_status(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let b = &cx.eng.persist.backup;
    Ok(Reply::Map(vec![
        (Reply::bulk("state"), Reply::bulk(b.state.name())),
        (Reply::bulk("error"), Reply::bulk_str(b.error.clone().unwrap_or_default())),
        (Reply::bulk("start_time"), Reply::Int(b.start)),
        (Reply::bulk("end_time"), Reply::Int(b.end)),
    ]))
}

/// BACKUP LIST: absolute paths of the files pinned so far.
pub fn backup_list(cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    let dir = crate::config::backupdirname(&cx.eng.cfg);
    let root = if dir.starts_with('/') { dir } else { format!("{}/{dir}", crate::config::DIR) };
    let b = &cx.eng.persist.backup;
    Ok(Reply::Array([&b.base, &b.incr, &b.manifest].into_iter().flatten().map(|f| Reply::bulk_str(format!("{root}/{f}"))).collect()))
}

/// BACKUP HELP
pub fn backup_help(_cx: &mut Cx, _args: &[Bytes]) -> CmdResult {
    Ok(help(&[
        "BACKUP <subcommand> [<arg> [value] [opt] ...]. Subcommands are:",
        "START",
        "    Start a new backup into the configured 'backupdirname'.",
        "SEAL",
        "    Freeze the current backup (BASE + INCR + manifest).",
        "ABORT",
        "    Cancel a backup that has not been sealed yet.",
        "CLEANUP",
        "    Remove a sealed backup's files and return to idle.",
        "STATUS",
        "    Report the current backup state.",
        "LIST",
        "    List the immutable files pinned so far.",
        "HELP",
        "    Return this help.",
        "HELP",
        "    Print this help.",
    ]))
}
