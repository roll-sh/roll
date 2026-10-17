//! `Cx`: what a command handler sees. It wraps the engine and the calling client and offers the
//! keyspace API every command uses, which takes care of Redis' bookkeeping:
//!
//! - lazy expiry on every access (with `expired` notifications and propagated DELs),
//! - LRU/LFU touches and hit/miss stats on reads,
//! - memory accounting, WATCH invalidation and blocked-client wakeups on writes (via [`KeyMut`]),
//! - deleting aggregates that become empty (with a `del` notification),
//! - propagation: a command that changed something is replicated as is, unless the handler
//!   supplies a deterministic rewrite with [`Cx::propagate_as`].

use crate::client::Client;
use crate::commands::{Command, Error, WRONGTYPE};
use crate::db::Db;
use crate::engine::{BlockReq, Effect, Engine, Mode, Signals};
use crate::notify;
use crate::types::{Array, Hash, List, Set, Stream, ZSet};
use crate::value::{Entry, Kind, Str, Value};
use bytes::Bytes;
use redis_proto::Reply;

pub struct Cx<'a> {
    pub eng: &'a mut Engine,
    pub client: &'a mut Client,
    /// Command time in Unix ms (frozen for the duration of EXEC and scripts).
    pub now: i64,
    pub cmd: &'static Command,
    pub mode: Mode,
    pub(crate) rewrite: Option<Vec<Bytes>>,
    pub(crate) block: Option<BlockReq>,
    pub(crate) no_propagate: bool,
    /// Lookups count keyspace hits/misses and emit `keymiss` ([`read_lookups`]).
    pub(crate) read_stats: bool,
}

/// Do the command's lookups count as reads (`lookupKeyRead`: keyspace hits/misses, `keymiss`)?
/// Write commands look keys up for writing (no stats), except those Redis reads through.
pub(crate) fn read_lookups(cmd: &Command, args: &[Bytes]) -> bool {
    if !cmd.is_write() {
        return true;
    }
    match cmd.name {
        "getdel" | "getex" | "copy" | "delex" | "bitop" | "pfmerge" | "sort" | "georadius" | "georadiusbymember" | "geosearchstore" | "sinterstore"
        | "sunionstore" | "sdiffstore" | "zunionstore" | "zinterstore" | "zdiffstore" | "zrangestore" | "xack" | "xackdel" | "xautoclaim" | "xclaim" => true,
        // SET … GET reads the old value like GET.
        "set" => args.iter().skip(3).any(|a| a.eq_ignore_ascii_case(b"get")),
        _ => false,
    }
}

/// Mutable access to a key's entry. On drop it updates memory accounting, signals the key as
/// modified and schedules deletion if an aggregate became empty.
pub struct KeyMut<'a> {
    pub entry: &'a mut Entry,
    mem: &'a mut usize,
    sig: &'a mut Signals,
    db: u16,
    key: Str,
    old: usize,
    changed: bool,
}

impl KeyMut<'_> {
    #[inline]
    pub fn value(&mut self) -> &mut Value {
        &mut self.entry.value
    }

    /// The operation turned out to be a no-op: don't signal or propagate.
    pub fn unchanged(&mut self) {
        self.changed = false;
    }

    /// Mark changed again after `unchanged()` (e.g. a later step did modify).
    pub fn changed(&mut self) {
        self.changed = true;
    }

    pub fn notify(&mut self, class: u32, event: &'static str) {
        self.sig.notify(class, event, self.db, &self.key);
    }

    pub fn key(&self) -> &[u8] {
        &self.key
    }

    pub fn str(&mut self) -> &mut Str {
        match &mut self.entry.value {
            Value::Str(s) => s,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn list(&mut self) -> &mut List {
        match &mut self.entry.value {
            Value::List(l) => l,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn hash(&mut self) -> &mut Hash {
        match &mut self.entry.value {
            Value::Hash(h) => h,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn set(&mut self) -> &mut Set {
        match &mut self.entry.value {
            Value::Set(s) => s,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn zset(&mut self) -> &mut ZSet {
        match &mut self.entry.value {
            Value::ZSet(z) => z,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn stream(&mut self) -> &mut Stream {
        match &mut self.entry.value {
            Value::Stream(s) => s,
            _ => unreachable!("type checked by lookup"),
        }
    }
    pub fn array(&mut self) -> &mut Array {
        match &mut self.entry.value {
            Value::Array(a) => a,
            _ => unreachable!("type checked by lookup"),
        }
    }
}

impl Drop for KeyMut<'_> {
    fn drop(&mut self) {
        let new = self.entry.value.mem();
        *self.mem = (*self.mem + new).saturating_sub(self.old);
        if self.changed {
            self.sig.touch(self.db, &self.key);
        }
        if self.entry.value.is_empty_aggregate() {
            self.sig.empty.push((self.db, std::mem::take(&mut self.key)));
        }
    }
}

macro_rules! typed_get {
    ($name:ident, $variant:ident, $ty:ty) => {
        /// Read a key of this type: `Ok(None)` when missing, WRONGTYPE for another type.
        pub fn $name(&mut self, key: &[u8]) -> Result<Option<&$ty>, Error> {
            match self.get(key) {
                None => Ok(None),
                Some(Value::$variant(v)) => Ok(Some(v)),
                Some(_) => Err(WRONGTYPE),
            }
        }
    };
}

impl<'a> Cx<'a> {
    pub(crate) fn new(eng: &'a mut Engine, client: &'a mut Client, now: i64, cmd: &'static Command, mode: Mode) -> Self {
        Self { eng, client, now, cmd, mode, rewrite: None, block: None, no_propagate: false, read_stats: true }
    }

    #[inline]
    pub fn db_index(&self) -> usize {
        self.client.db
    }

    pub fn db(&mut self) -> &mut Db {
        &mut self.eng.dbs[self.client.db]
    }

    fn expire_check(&mut self, key: &[u8]) -> bool {
        if !self.eng.sig.empty.is_empty() {
            self.eng.flush_empty();
        }
        self.eng.expire_if_needed(self.client.db, key, self.now, self.mode)
    }

    /// Look up a key for reading. Updates LRU/LFU and hit/miss stats.
    pub fn get(&mut self, key: &[u8]) -> Option<&Value> {
        self.get_entry(key).map(|e| &e.value)
    }

    pub fn get_entry(&mut self, key: &[u8]) -> Option<&Entry> {
        let expired = self.expire_check(key);
        let now = self.now;
        let no_touch = self.client.no_touch;
        let read = self.read_stats;
        let Engine { dbs, cfg, rng, stats, sig, .. } = &mut *self.eng;
        let found = if expired { None } else { dbs[self.client.db].keys.get_mut(key) };
        match found {
            Some(e) => {
                if read {
                    stats.keyspace_hits += 1;
                }
                if !no_touch {
                    Engine::touch_entry(cfg, rng, e, now);
                }
                Some(&*e)
            }
            None => {
                if read {
                    stats.keyspace_misses += 1;
                    sig.notify(notify::KEY_MISS, "keymiss", self.client.db as u16, key);
                }
                None
            }
        }
    }

    /// Look up without touching LRU or stats (TYPE-like introspection, internal checks).
    pub fn peek(&mut self, key: &[u8]) -> Option<&Entry> {
        if self.expire_check(key) {
            return None;
        }
        self.eng.dbs[self.client.db].keys.get(key)
    }

    pub fn exists(&mut self, key: &[u8]) -> bool {
        self.get(key).is_some()
    }

    typed_get!(get_str, Str, Str);
    typed_get!(get_list, List, List);
    typed_get!(get_hash, Hash, Hash);
    typed_get!(get_set, Set, Set);
    typed_get!(get_zset, ZSet, ZSet);
    typed_get!(get_stream, Stream, Stream);
    typed_get!(get_array, Array, Array);

    /// Mutable access to an existing key (any type).
    pub fn get_mut(&mut self, key: &[u8]) -> Option<KeyMut<'_>> {
        if self.expire_check(key) {
            return None;
        }
        let now = self.now;
        let no_touch = self.client.no_touch;
        let db = self.client.db;
        let Engine { dbs, cfg, rng, sig, .. } = &mut *self.eng;
        let Db { keys, mem, .. } = &mut dbs[db];
        let entry = keys.get_mut(key)?;
        if cfg.maxmemory_policy.is_lrm() {
            entry.lru = Engine::lru_clock(now);
        } else if !no_touch {
            Engine::touch_entry(cfg, rng, entry, now);
        }
        let old = entry.value.mem();
        Some(KeyMut { entry, mem, sig, db: db as u16, key: Str::from_slice(key), old, changed: true })
    }

    /// Mutable access to an existing key of `kind` (`Ok(None)` if missing).
    pub fn get_mut_typed(&mut self, key: &[u8], kind: Kind) -> Result<Option<KeyMut<'_>>, Error> {
        match self.peek(key) {
            None => return Ok(None),
            Some(e) if e.value.kind() != kind => return Err(WRONGTYPE),
            _ => {}
        }
        Ok(self.get_mut(key))
    }

    /// Check a key's type without touching it: `Ok(false)` if missing, WRONGTYPE if different.
    pub fn check_type(&mut self, key: &[u8], kind: Kind) -> Result<bool, Error> {
        match self.peek(key) {
            None => Ok(false),
            Some(e) if e.value.kind() == kind => Ok(true),
            Some(_) => Err(WRONGTYPE),
        }
    }

    /// Get a key of `kind`, creating an empty one if missing. WRONGTYPE for another type.
    pub fn get_or_create(&mut self, key: &[u8], kind: Kind) -> Result<KeyMut<'_>, Error> {
        if !self.check_type(key, kind)? {
            let lru = self.eng.new_lru(self.now);
            let db = self.client.db;
            self.eng.dbs[db].insert_new(key, Entry::new(Value::new_of(kind), lru));
            self.eng.sig.notify(notify::NEW, "new", db as u16, key);
        }
        Ok(self.get_mut(key).expect("just created"))
    }

    /// Create or overwrite a key. `keep_ttl` keeps an existing TTL (SET KEEPTTL); otherwise any
    /// TTL is removed.
    pub fn set(&mut self, key: &[u8], value: Value, keep_ttl: bool) {
        self.expire_check(key);
        let db = self.client.db;
        let lru = self.eng.new_lru(self.now);
        let d = &mut self.eng.dbs[db];
        match d.keys.get_mut(key) {
            Some(e) => {
                let old = e.value.mem();
                let new = value.mem();
                let hashes = matches!(e.value, Value::Hash(_)) || matches!(value, Value::Hash(_));
                e.value = value;
                e.lru = lru;
                let had_ttl = e.expire != 0;
                d.mem = (d.mem + new).saturating_sub(old);
                if had_ttl && !keep_ttl {
                    d.set_expire(key, 0);
                }
                if hashes {
                    // A hash with field TTLs replaced (or installed): keep `subexpires` in sync.
                    d.sync_subexpiry(key);
                }
            }
            None => {
                d.insert_new(key, Entry::new(value, lru));
                self.eng.sig.notify(notify::NEW, "new", db as u16, key);
            }
        }
        self.eng.sig.touch(db as u16, key);
    }

    /// Create or overwrite a key with an absolute expiry (0 = none).
    pub fn set_with_expire(&mut self, key: &[u8], value: Value, expire_at: i64) {
        self.set(key, value, false);
        if expire_at != 0 {
            let db = self.client.db;
            self.eng.dbs[db].set_expire(key, expire_at);
        }
    }

    /// Delete a key. The caller emits the `del` notification when appropriate.
    pub fn delete(&mut self, key: &[u8]) -> bool {
        if self.expire_check(key) {
            return false;
        }
        let db = self.client.db;
        if self.eng.dbs[db].remove(key).is_some() {
            self.eng.sig.touch(db as u16, key);
            true
        } else {
            false
        }
    }

    /// Remove a key and return its entry (RENAME, MOVE, GETDEL…).
    pub fn take(&mut self, key: &[u8]) -> Option<Entry> {
        if self.expire_check(key) {
            return None;
        }
        let db = self.client.db;
        let e = self.eng.dbs[db].remove(key)?;
        self.eng.sig.touch(db as u16, key);
        Some(e)
    }

    /// Insert a whole entry into database `db` (overwriting), keeping its expiry.
    pub fn put_entry(&mut self, db: usize, key: &[u8], entry: Entry) {
        let d = &mut self.eng.dbs[db];
        let existed = d.remove(key).is_some();
        d.insert_new(key, entry);
        if !existed {
            self.eng.sig.notify(notify::NEW, "new", db as u16, key);
        }
        self.eng.sig.touch(db as u16, key);
    }

    /// The key's absolute expiry: `None` if missing, `Some(0)` if it has no TTL.
    pub fn expire_of(&mut self, key: &[u8]) -> Option<i64> {
        self.peek(key).map(|e| e.expire)
    }

    /// Set (or clear with 0) a key's absolute expiry. False if the key is missing.
    pub fn set_expire(&mut self, key: &[u8], at: i64) -> bool {
        if self.expire_check(key) {
            return false;
        }
        let db = self.client.db;
        if self.eng.dbs[db].set_expire(key, at) {
            self.eng.sig.touch(db as u16, key);
            true
        } else {
            false
        }
    }

    /// Emit a keyspace notification for `key` in the current database.
    pub fn notify(&mut self, class: u32, event: &'static str, key: &[u8]) {
        self.eng.sig.notify(class, event, self.client.db as u16, key);
    }

    /// Signal `key` as modified (WATCH, blocked clients, propagation) without going through a
    /// `KeyMut`.
    pub fn touch(&mut self, key: &[u8]) {
        self.eng.sig.touch(self.client.db as u16, key);
    }

    /// Propagate these arguments instead of the original command.
    pub fn propagate_as(&mut self, args: Vec<Bytes>) {
        self.rewrite = Some(args);
    }

    /// Don't propagate the command itself (its effects were propagated individually).
    pub fn no_propagate(&mut self) {
        self.no_propagate = true;
    }

    /// Propagate an extra effect now (before the command's own).
    pub fn also_propagate(&mut self, db: usize, args: Vec<Bytes>) {
        self.eng.effects.push(Effect { db: db as u16, args });
    }

    /// Can this command block (not inside MULTI, a script, or a replayed effect)?
    pub fn can_block(&self) -> bool {
        matches!(self.mode, Mode::Top | Mode::Unblock)
    }

    /// Block on `keys` until one changes or `timeout_ms` passes (0 = forever; the server enforces
    /// it). Returns the reply to send now: when blocking isn't possible, `timeout_reply` itself.
    pub fn block_on(&mut self, keys: Vec<Str>, timeout_ms: i64, timeout_reply: Reply) -> Reply {
        if !self.can_block() {
            return timeout_reply;
        }
        let deadline = if timeout_ms > 0 { self.now.saturating_add(timeout_ms) } else { 0 };
        self.block = Some(BlockReq { keys, timeout_reply, deadline, args: None, unblock_on_nokey: false, wake_at: 0 });
        Reply::Nil
    }

    /// [`Cx::block_on`] with extras: re-run with `args` instead of the original arguments, also
    /// wake when a key is deleted, and/or re-run at `wake_at` (absolute Unix ms, 0 = never).
    /// When re-blocking from a re-run, the original deadline is kept.
    pub fn block_on_ex(
        &mut self,
        keys: Vec<Str>,
        timeout_ms: i64,
        timeout_reply: Reply,
        args: Option<Vec<Bytes>>,
        unblock_on_nokey: bool,
        wake_at: i64,
    ) -> Reply {
        let r = self.block_on(keys, timeout_ms, timeout_reply);
        if let Some(b) = self.block.as_mut() {
            b.args = args;
            b.unblock_on_nokey = unblock_on_nokey;
            b.wake_at = wake_at;
        }
        r
    }

    /// Wake clients blocked on `key` (they re-run their command) without signalling it as
    /// modified for WATCH.
    pub fn signal_ready(&mut self, key: &[u8]) {
        let k = (self.client.db as u16, Str::from_slice(key));
        let b = &mut self.eng.blocking;
        if b.by_key.contains_key(&k) && b.ready_set.insert(k.clone()) {
            b.ready.push(k);
        }
    }

    /// Run another command as this client (used by EXEC and scripts).
    pub fn call(&mut self, args: &[Bytes], mode: Mode) -> Reply {
        let now = self.now;
        match self.eng.dispatch(self.client, args, now, mode) {
            crate::engine::Outcome::Reply(r) => r,
            crate::engine::Outcome::Blocked => Reply::Nil,
        }
    }

    pub fn cfg(&self) -> &crate::config::Config {
        &self.eng.cfg
    }

    pub fn resp(&self) -> u8 {
        self.client.resp
    }
}
