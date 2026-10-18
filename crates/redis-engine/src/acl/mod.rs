//! ACL users, permission checks and the ACL LOG, with Redis 8 semantics.
//!
//! # Model
//!
//! - [`Acl`] is one database's registry: users by name (always including `default`) and the ACL
//!   LOG. All shard engines of a database share it (`Engine::acl`, an `Arc<RwLock<Acl>>`).
//! - [`User`] is shared by the registry and every client authenticated as it (`client.user`).
//!   Its rules live behind a lock inside the `User`, so `ACL SETUSER` changes them in place and
//!   connected clients see the new permissions on their next command, like Redis.
//!   `ACL DELUSER` marks the user deleted and notifies the connections watching it
//!   ([`User::watch`] / [`crate::hub::Sink::acl_changed`]); they disconnect (see
//!   [`must_disconnect`]). Pub/Sub clients whose subscriptions are no longer allowed are
//!   disconnected the same way.
//!
//! # Hot path
//!
//! [`User::check`] runs before every command. Users whose root selector allows every command,
//! key and channel (the default user) are recognised with a single atomic load. Otherwise the
//! check takes the user's read lock and tests a precomputed bitmap (one bit per command and
//! subcommand, see [`index`]) and the key/channel patterns; key positions of range-key commands
//! are computed without allocating.

mod index;
mod selector;
mod spec;

pub use index::{CATEGORIES, categories_of, category_bit, category_names};
pub use selector::{Reason, RuleError};

use crate::client::Client;
use crate::commands::Command;
use crate::engine::Mode;
use crate::hub::Sink;
use bytes::Bytes;
use index::index;
use parking_lot::{Mutex, RwLock};
use redis_proto::Reply;
use selector::{Keys, Selector, has_spaces};
use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};

/// Maximum ACL LOG entries (`acllog-max-len`).
pub const LOG_MAX_LEN: usize = 128;
/// Entries similar to an existing one within this window update it instead (ms).
const LOG_GROUPING_MS: i64 = 60_000;

/// A user's rules: flags, passwords and selectors (`selectors[0]` is the root selector).
#[derive(Clone, Debug)]
pub struct Rules {
    pub enabled: bool,
    pub nopass: bool,
    pub skip_sanitize: bool,
    /// SHA-256 hex digests.
    pub passwords: Vec<String>,
    selectors: Vec<Selector>,
}

impl Rules {
    /// A new user: `off sanitize-payload resetchannels -@all`.
    pub fn new() -> Self {
        Self { enabled: false, nopass: false, skip_sanitize: false, passwords: Vec::new(), selectors: vec![Selector::new(true)] }
    }

    /// The default user: `on nopass ~* &* +@all`.
    pub fn default_user() -> Self {
        let mut r = Self::new();
        for op in ["+@all", "~*", "&*", "on", "nopass"] {
            r.apply(op.as_bytes()).expect("default user rules");
        }
        r
    }

    fn root(&self) -> &Selector {
        &self.selectors[0]
    }

    fn unrestricted(&self) -> bool {
        self.root().unrestricted()
    }

    /// Apply one rule (`ACLSetUser`).
    pub fn apply(&mut self, op: &[u8]) -> Result<(), RuleError> {
        if op.is_empty() {
            return Ok(());
        }
        let is = |s: &str| op.eq_ignore_ascii_case(s.as_bytes());
        if is("on") {
            self.enabled = true;
        } else if is("off") {
            self.enabled = false;
        } else if is("skip-sanitize-payload") {
            self.skip_sanitize = true;
        } else if is("sanitize-payload") {
            self.skip_sanitize = false;
        } else if is("nopass") {
            self.nopass = true;
            self.passwords.clear();
        } else if is("resetpass") {
            self.nopass = false;
            self.passwords.clear();
        } else if op[0] == b'>' || op[0] == b'#' {
            let hash = if op[0] == b'>' { hash_password(&op[1..]) } else { valid_hash(&op[1..])? };
            if !self.passwords.contains(&hash) {
                self.passwords.push(hash);
            }
            self.nopass = false;
        } else if op[0] == b'<' || op[0] == b'!' {
            let hash = if op[0] == b'<' { hash_password(&op[1..]) } else { valid_hash(&op[1..])? };
            let i = self.passwords.iter().position(|p| *p == hash).ok_or(RuleError::NoSuchPassword)?;
            self.passwords.remove(i);
        } else if op[0] == b'(' && op[op.len() - 1] == b')' {
            let inner = &op[1..op.len() - 1];
            let mut s = Selector::new(false);
            for rule in redis_proto::split_args(inner).unwrap_or_default() {
                s.apply(&rule)?;
            }
            self.selectors.push(s);
        } else if is("clearselectors") {
            self.selectors.truncate(1);
        } else if is("reset") {
            for r in ["resetpass", "resetkeys", "resetchannels", "off", "sanitize-payload", "clearselectors", "-@all"] {
                self.apply(r.as_bytes()).expect("reset rules");
            }
        } else {
            self.selectors[0].apply(op)?;
        }
        Ok(())
    }

    /// Flag names, in Redis' order.
    pub fn flags(&self) -> Vec<&'static str> {
        let mut f = vec![if self.enabled { "on" } else { "off" }];
        if self.nopass {
            f.push("nopass");
        }
        f.push(if self.skip_sanitize { "skip-sanitize-payload" } else { "sanitize-payload" });
        f
    }

    /// `ACLDescribeUser`: the rules as `ACL LIST` shows them (after `user <name> `).
    pub fn describe(&self) -> Vec<u8> {
        let mut out = Vec::new();
        for f in self.flags() {
            out.extend_from_slice(f.as_bytes());
            out.push(b' ');
        }
        for p in &self.passwords {
            out.push(b'#');
            out.extend_from_slice(p.as_bytes());
            out.push(b' ');
        }
        for s in &self.selectors {
            if s.root {
                out.extend_from_slice(&s.describe());
            } else {
                out.extend_from_slice(b" (");
                out.extend_from_slice(&s.describe());
                out.push(b')');
            }
        }
        out
    }

    /// `ACL GETUSER` reply.
    pub fn getuser_reply(&self) -> Reply {
        fn sel(s: &Selector) -> Vec<(Reply, Reply)> {
            vec![
                (Reply::bulk("commands"), Reply::Bulk(s.describe_commands().into())),
                (Reply::bulk("keys"), Reply::Bulk(s.describe_keys().into())),
                (Reply::bulk("channels"), Reply::Bulk(s.describe_channels().into())),
            ]
        }
        let mut m = vec![
            (Reply::bulk("flags"), Reply::Set(self.flags().into_iter().map(Reply::bulk).collect())),
            (Reply::bulk("passwords"), Reply::Array(self.passwords.iter().map(Reply::bulk).collect())),
        ];
        m.extend(sel(self.root()));
        m.push((Reply::bulk("selectors"), Reply::Array(self.selectors[1..].iter().map(|s| Reply::Map(sel(s))).collect())));
        Reply::Map(m)
    }

    /// `ACLCheckAllUserCommandPerm`.
    fn check(&self, cmd: &Command, args: &[Bytes]) -> Result<(), Denied> {
        if self.unrestricted() {
            return Ok(());
        }
        let Some(m) = index().meta(cmd) else { return Err(Denied { reason: Reason::Command, pos: 0 }) };
        let mut keys = Keys::default();
        let mut worst = (Reason::Command, 0usize);
        for s in &self.selectors {
            match s.check(m, args, &mut keys) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if e.0 > worst.0 || (e.0 == worst.0 && e.1 > worst.1) {
                        worst = e;
                    }
                }
            }
        }
        Err(Denied { reason: worst.0, pos: worst.1 })
    }

    /// `ACLUserCheckCmdWithUnrestrictedKeyAccess` for reads: does a selector that allows the
    /// command also allow reading every key (`allkeys`, or a `*` pattern with read access)?
    fn full_key_access(&self, cmd: &Command, args: &[Bytes]) -> bool {
        if self.unrestricted() {
            return true;
        }
        let Some(m) = index().meta(cmd) else { return false };
        let mut keys = Keys::default();
        self.selectors.iter().any(|s| {
            s.check(m, args, &mut keys).is_ok() && (s.allkeys || s.keys.iter().any(|k| k.perm & index::PERM_R != 0 && k.pattern == b"*"))
        })
    }

    /// `ACLUserCheckKeyPerm(..., CMD_KEY_ACCESS)`: may a selector read `key`?
    fn can_read_key(&self, key: &[u8]) -> bool {
        self.key_perm(key, index::PERM_R)
    }

    /// `ACLUserCheckKeyPerm`: does a selector grant every permission in `need` on `key`?
    fn key_perm(&self, key: &[u8], need: u8) -> bool {
        self.selectors.iter().any(|s| s.allkeys || s.keys.iter().any(|k| k.perm & need == need && crate::util::glob_match(&k.pattern, key, false)))
    }

    /// `ACLUserCheckKeyPerm` with `CMD_KEY_PREFIX`: does a selector grant `need` on every key
    /// starting with `prefix` (a pattern matching the prefix itself and ending in an unescaped `*`)?
    fn prefix_perm(&self, prefix: &[u8], need: u8) -> bool {
        let open_ended = |p: &[u8]| p.last() == Some(&b'*') && p[..p.len() - 1].iter().rev().take_while(|&&c| c == b'\\').count() % 2 == 0;
        self.selectors.iter().any(|s| {
            s.allkeys || s.keys.iter().any(|k| k.perm & need == need && crate::util::glob_match(&k.pattern, prefix, false) && open_ended(&k.pattern))
        })
    }

    /// May a client subscribed to these channels keep its subscriptions?
    fn subscriptions_allowed(&self, client: &Client) -> bool {
        let s = &self.selectors;
        client.psubs.iter().all(|p| selector::channels_allow(s, p, true))
            && client.subs.iter().all(|c| selector::channels_allow(s, c, false))
            && client.ssubs.iter().all(|c| selector::channels_allow(s, c, false))
    }
}

impl Default for Rules {
    fn default() -> Self {
        Self::new()
    }
}

pub fn hash_password(p: &[u8]) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(p);
    let mut out = String::with_capacity(64);
    for b in digest.iter() {
        out.push(char::from(b"0123456789abcdef"[(b >> 4) as usize]));
        out.push(char::from(b"0123456789abcdef"[(b & 15) as usize]));
    }
    out
}

fn valid_hash(h: &[u8]) -> Result<String, RuleError> {
    if h.len() != 64 || !h.iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c)) {
        return Err(RuleError::BadHash);
    }
    Ok(String::from_utf8_lossy(h).into_owned())
}

/// Why a command was refused, and which argument caused it (0 = the command itself).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Denied {
    pub reason: Reason,
    pub pos: usize,
}

/// A user, shared by the registry and the clients authenticated as it.
pub struct User {
    pub name: String,
    rules: RwLock<Rules>,
    /// Cached `rules.unrestricted()`: the check's fast path.
    open: AtomicBool,
    deleted: AtomicBool,
    /// Connections authenticated as this user, notified when it changes or is deleted.
    watchers: Mutex<Vec<(u64, Weak<dyn Sink>)>>,
    /// The placeholder default user of clients not yet bound to a registry.
    placeholder: bool,
}

impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User").field("name", &self.name).finish()
    }
}

impl User {
    fn new(name: &[u8], rules: Rules) -> Arc<User> {
        Arc::new(User {
            name: String::from_utf8_lossy(name).into_owned(),
            open: AtomicBool::new(rules.unrestricted()),
            rules: RwLock::new(rules),
            deleted: AtomicBool::new(false),
            watchers: Mutex::new(Vec::new()),
            placeholder: false,
        })
    }

    /// The default user for clients created outside a database (`Client::new`). The engine
    /// replaces it with its registry's `default` user on the client's first command.
    pub fn default_user() -> Arc<User> {
        static DEFAULT: OnceLock<Arc<User>> = OnceLock::new();
        DEFAULT
            .get_or_init(|| {
                let rules = Rules::default_user();
                Arc::new(User {
                    name: "default".into(),
                    open: AtomicBool::new(true),
                    rules: RwLock::new(rules),
                    deleted: AtomicBool::new(false),
                    watchers: Mutex::new(Vec::new()),
                    placeholder: true,
                })
            })
            .clone()
    }

    pub fn is_placeholder(&self) -> bool {
        self.placeholder
    }

    /// May this user run `cmd` (the resolved command or subcommand) with `args`?
    #[inline]
    pub fn check(&self, cmd: &Command, args: &[Bytes]) -> Result<(), Denied> {
        if self.open.load(Ordering::Acquire) {
            return Ok(());
        }
        self.rules.read().check(cmd, args)
    }

    /// May this user's `cmd` read keys it names only indirectly (SORT's BY and GET patterns)?
    pub fn full_key_access(&self, cmd: &Command, args: &[Bytes]) -> bool {
        self.open.load(Ordering::Acquire) || self.rules.read().full_key_access(cmd, args)
    }

    /// May this user read `key` (BCAST invalidations are filtered by it)?
    pub fn can_read_key(&self, key: &[u8]) -> bool {
        self.open.load(Ordering::Acquire) || self.rules.read().can_read_key(key)
    }

    /// `RM_ACLCheckKeyPermissions`: may this user read (`write` false) or update `key`?
    pub fn can_use_key(&self, key: &[u8], write: bool) -> bool {
        self.open.load(Ordering::Acquire) || self.rules.read().key_perm(key, if write { index::PERM_W } else { index::PERM_R })
    }

    /// `RM_ACLCheckKeyPrefixPermissions(..., CMD_KEY_ACCESS)`: may this user read every key
    /// under `prefix`?
    pub fn can_access_prefix(&self, prefix: &[u8]) -> bool {
        self.open.load(Ordering::Acquire) || self.rules.read().prefix_perm(prefix, index::PERM_R)
    }

    /// A copy of the current rules.
    pub fn rules(&self) -> Rules {
        self.rules.read().clone()
    }

    pub fn enabled(&self) -> bool {
        self.rules.read().enabled
    }

    pub fn nopass(&self) -> bool {
        self.rules.read().nopass
    }

    /// Removed with `ACL DELUSER`: clients authenticated as it must disconnect.
    pub fn is_deleted(&self) -> bool {
        self.deleted.load(Ordering::Acquire)
    }

    fn set_rules(&self, rules: Rules) {
        let open = rules.unrestricted();
        let mut w = self.rules.write();
        *w = rules;
        self.open.store(open, Ordering::Release);
    }

    /// Register a connection authenticated as this user, to be told (through
    /// [`Sink::acl_changed`]) when the user is deleted or loses channel permissions.
    pub fn watch(&self, client_id: u64, sink: &Arc<dyn Sink>) {
        let mut w = self.watchers.lock();
        w.retain(|(id, s)| *id != client_id && s.strong_count() > 0);
        w.push((client_id, Arc::downgrade(sink)));
    }

    pub fn unwatch(&self, client_id: u64) {
        self.watchers.lock().retain(|(id, _)| *id != client_id);
    }

    fn notify(&self) {
        let sinks: Vec<_> = self.watchers.lock().iter().filter_map(|(_, s)| s.upgrade()).collect();
        for s in sinks {
            s.acl_changed();
        }
    }
}

/// Must this connection be closed after an ACL change? (Its user was deleted, or it is
/// subscribed to channels its user may no longer access.)
pub fn must_disconnect(client: &Client) -> bool {
    let u = &client.user;
    if u.is_deleted() {
        return true;
    }
    client.subscriptions() > 0 && !u.rules.read().subscriptions_allowed(client)
}

/// Where a denied command ran, for ACL LOG.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Toplevel,
    Multi,
    Lua,
}

impl Context {
    fn name(self) -> &'static str {
        match self {
            Context::Toplevel => "toplevel",
            Context::Multi => "multi",
            Context::Lua => "lua",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogReason {
    Command,
    Key,
    Channel,
    Auth,
}

impl LogReason {
    fn name(self) -> &'static str {
        match self {
            LogReason::Command => "command",
            LogReason::Key => "key",
            LogReason::Channel => "channel",
            LogReason::Auth => "auth",
        }
    }
}

impl From<Reason> for LogReason {
    fn from(r: Reason) -> Self {
        match r {
            Reason::Command => LogReason::Command,
            Reason::Key => LogReason::Key,
            Reason::Channel => LogReason::Channel,
        }
    }
}

#[derive(Clone, Debug)]
pub struct LogEntry {
    pub count: u64,
    pub reason: LogReason,
    pub context: Context,
    pub object: Bytes,
    pub username: Bytes,
    /// Last update (Unix ms).
    pub ctime: i64,
    pub client_info: String,
    pub entry_id: u64,
    pub created: i64,
}

#[derive(Debug)]
pub struct AclLog {
    /// Newest first.
    pub entries: VecDeque<LogEntry>,
    next_id: u64,
    pub max_len: usize,
    /// INFO `acl_access_denied_*` counters: auth, cmd, key, channel.
    pub denied: [u64; 4],
}

impl Default for AclLog {
    fn default() -> Self {
        Self { entries: VecDeque::new(), next_id: 0, max_len: LOG_MAX_LEN, denied: [0; 4] }
    }
}

impl AclLog {
    /// `addACLLogEntry`.
    pub fn add(&mut self, mut e: LogEntry) {
        self.denied[match e.reason {
            LogReason::Auth => 0,
            LogReason::Command => 1,
            LogReason::Key => 2,
            LogReason::Channel => 3,
        }] += 1;
        if self.max_len == 0 {
            self.entries.clear();
            return;
        }
        let similar = self.entries.iter().take(10).position(|o| {
            o.reason == e.reason
                && o.context == e.context
                && (o.ctime - e.ctime).abs() <= LOG_GROUPING_MS
                && o.object == e.object
                && o.username == e.username
        });
        if let Some(i) = similar {
            let mut o = self.entries.remove(i).unwrap();
            o.client_info = e.client_info;
            o.ctime = e.ctime;
            o.count += 1;
            self.entries.push_front(o);
            return;
        }
        e.entry_id = self.next_id;
        self.next_id += 1;
        self.entries.push_front(e);
        self.entries.truncate(self.max_len);
    }

    /// `ACL LOG [count]` reply.
    pub fn reply(&self, count: usize, now: i64) -> Reply {
        Reply::Array(
            self.entries
                .iter()
                .take(count)
                .map(|e| {
                    Reply::Map(vec![
                        (Reply::bulk("count"), Reply::Int(e.count as i64)),
                        (Reply::bulk("reason"), Reply::bulk(e.reason.name())),
                        (Reply::bulk("context"), Reply::bulk(e.context.name())),
                        (Reply::bulk("object"), Reply::Bulk(e.object.clone())),
                        (Reply::bulk("username"), Reply::Bulk(e.username.clone())),
                        (Reply::bulk("age-seconds"), Reply::Double((now - e.ctime) as f64 / 1000.0)),
                        (Reply::bulk("client-info"), Reply::bulk(&e.client_info)),
                        (Reply::bulk("entry-id"), Reply::Int(e.entry_id as i64)),
                        (Reply::bulk("timestamp-created"), Reply::Int(e.created)),
                        (Reply::bulk("timestamp-last-updated"), Reply::Int(e.ctime)),
                    ])
                })
                .collect(),
        )
    }
}

/// Authentication failures, with Redis' error text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthFailure {
    /// Unknown user, disabled user or wrong password.
    WrongPass,
    /// `AUTH <password>` while the default user needs no password.
    NoPasswordConfigured,
}

impl std::fmt::Display for AuthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            AuthFailure::WrongPass => "WRONGPASS invalid username-password pair or user is disabled.",
            AuthFailure::NoPasswordConfigured => {
                "ERR AUTH <password> called without any password configured for the default user. Are you sure your configuration is correct?"
            }
        })
    }
}

/// One database's users and ACL LOG.
pub struct Acl {
    users: BTreeMap<Vec<u8>, Arc<User>>,
    default: Arc<User>,
    pub log: Mutex<AclLog>,
}

impl Default for Acl {
    fn default() -> Self {
        Self::new()
    }
}

impl Acl {
    pub fn new() -> Self {
        let default = User::new(b"default", Rules::default_user());
        let mut users = BTreeMap::new();
        users.insert(b"default".to_vec(), default.clone());
        Self { users, default, log: Mutex::new(AclLog::default()) }
    }

    pub fn default_user(&self) -> Arc<User> {
        self.default.clone()
    }

    pub fn user(&self, name: &[u8]) -> Option<&Arc<User>> {
        self.users.get(name)
    }

    /// Users in name order.
    pub fn users(&self) -> impl Iterator<Item = (&[u8], &Arc<User>)> {
        self.users.iter().map(|(k, v)| (k.as_slice(), v))
    }

    /// `requirepass`: set (or with `None`, remove) the default user's only password.
    pub fn set_default_password(&mut self, password: Option<&[u8]>) {
        let mut r = self.default.rules();
        r.apply(b"resetpass").unwrap();
        match password {
            Some(p) => {
                let mut op = b">".to_vec();
                op.extend_from_slice(p);
                r.apply(&op).unwrap();
            }
            None => r.apply(b"nopass").unwrap(),
        }
        self.default.set_rules(r);
    }

    /// Check credentials (`AUTH`, `HELLO … AUTH`). `user: None` is the one-argument `AUTH
    /// <password>` form, for the default user. Failures are not logged here: see
    /// [`Acl::log_auth_failure`].
    pub fn authenticate(&self, user: Option<&[u8]>, password: &[u8]) -> Result<Arc<User>, AuthFailure> {
        if user.is_none() && self.default.nopass() {
            return Err(AuthFailure::NoPasswordConfigured);
        }
        let u = self.users.get(user.unwrap_or(b"default")).ok_or(AuthFailure::WrongPass)?;
        let r = u.rules.read();
        if !r.enabled {
            return Err(AuthFailure::WrongPass);
        }
        if r.nopass {
            return Ok(u.clone());
        }
        let hashed = hash_password(password);
        if r.passwords.iter().any(|p| ct_eq(p.as_bytes(), hashed.as_bytes())) {
            return Ok(u.clone());
        }
        Err(AuthFailure::WrongPass)
    }

    /// Record a failed authentication of `client` (`command` is the command name as sent).
    pub fn log_auth_failure(&self, client: &Client, command: &[u8], username: &[u8]) {
        let ctx = if client.in_multi() { Context::Multi } else { Context::Toplevel };
        self.log_entry(client, LogReason::Auth, ctx, Bytes::copy_from_slice(command), Bytes::copy_from_slice(username));
    }

    fn log_entry(&self, client: &Client, reason: LogReason, context: Context, object: Bytes, username: Bytes) {
        let now = crate::util::now_ms();
        let view = crate::clients::View::of(client);
        let derived = crate::commands::client::Derived::default();
        let client_info = crate::commands::client::line_with(client.id, &view, &derived, client.conn.as_deref(), None, now);
        self.log.lock().add(LogEntry {
            count: 1,
            reason,
            context,
            object,
            username,
            ctime: now,
            client_info,
            entry_id: 0,
            created: now,
        });
    }

    /// `ACL SETUSER name rules…` (the name was validated by the caller). Returns the error
    /// message (without `ERR`).
    pub fn set_user(&mut self, name: &[u8], ops: &[Bytes]) -> Result<(), String> {
        let ops = merge_selectors(ops)?;
        let existing = self.users.get(name).cloned();
        let mut rules = existing.as_ref().map_or_else(Rules::new, |u| u.rules());
        for op in &ops {
            if let Err(e) = rules.apply(op) {
                return Err(format!("Error in ACL SETUSER modifier '{}': {}", redis_proto::escape(cstr(op)), e.message()));
            }
        }
        match existing {
            Some(u) => {
                let lost_channels = !selector::channels_superset(&rules.selectors, &u.rules.read().selectors);
                u.set_rules(rules);
                if lost_channels {
                    u.notify();
                }
            }
            None => {
                self.users.insert(name.to_vec(), User::new(name, rules));
            }
        }
        Ok(())
    }

    /// `ACL DELUSER`: the number of users deleted. Their clients are told to disconnect.
    pub fn del_users(&mut self, names: &[Bytes]) -> Result<i64, &'static str> {
        if names.iter().any(|n| &n[..] == b"default") {
            return Err("ERR The 'default' user cannot be removed");
        }
        let mut n = 0;
        for name in names {
            if let Some(u) = self.users.remove(&name[..]) {
                u.deleted.store(true, Ordering::Release);
                u.notify();
                n += 1;
            }
        }
        Ok(n)
    }

    /// Delete every user but `default` and reset it to `on nopass ~* &* +@all` (tests, tenant
    /// reset).
    pub fn reset(&mut self) {
        let names: Vec<Bytes> = self.users.keys().filter(|k| &k[..] != b"default").map(|k| Bytes::copy_from_slice(k)).collect();
        let _ = self.del_users(&names);
        self.default.set_rules(Rules::default_user());
        let mut log = self.log.lock();
        log.entries.clear();
    }
}

/// C-string view (Redis formats rules with `%s`).
fn cstr(b: &[u8]) -> &[u8] {
    b.iter().position(|&c| c == 0).map_or(b, |i| &b[..i])
}

fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |d, (x, y)| d | (x ^ y)) == 0
}

/// `ACLMergeSelectorArguments`: a selector may be spread over several arguments.
fn merge_selectors(ops: &[Bytes]) -> Result<Vec<Vec<u8>>, String> {
    let mut out = Vec::with_capacity(ops.len());
    let mut open: Option<(usize, Vec<u8>)> = None;
    for (i, op) in ops.iter().enumerate() {
        if let Some((_, sel)) = open.as_mut() {
            sel.push(b' ');
            sel.extend_from_slice(cstr(op));
            if op.last() == Some(&b')') {
                out.push(open.take().unwrap().1);
            }
            continue;
        }
        if op.first() == Some(&b'(') && op.last() != Some(&b')') {
            open = Some((i, op.to_vec()));
            continue;
        }
        out.push(op.to_vec());
    }
    if let Some((i, _)) = open {
        return Err(format!("Unmatched parenthesis in acl selector starting at '{}'.", redis_proto::escape(cstr(&ops[i]))));
    }
    Ok(out)
}

/// Validate a user name for ACL SETUSER.
pub fn valid_username(name: &[u8]) -> bool {
    !has_spaces(name)
}

/// The error reply for a denied command, after recording it in ACL LOG.
pub fn deny(acl: &RwLock<Acl>, client: &Client, full_name: &str, args: &[Bytes], d: Denied, mode: Mode) -> Reply {
    let context = match mode {
        Mode::Exec => Context::Multi,
        Mode::Script { .. } => Context::Lua,
        _ if client.in_multi() => Context::Multi,
        _ => Context::Toplevel,
    };
    let object = match d.reason {
        Reason::Command => Bytes::copy_from_slice(full_name.as_bytes()),
        _ => args[d.pos].clone(),
    };
    acl.read().log_entry(client, d.reason.into(), context, object, Bytes::copy_from_slice(client.user.name.as_bytes()));
    let msg = match d.reason {
        Reason::Command => format!("User {} has no permissions to run the '{}' command", client.user.name, full_name),
        Reason::Key => "No permissions to access a key".to_string(),
        Reason::Channel => "No permissions to access a channel".to_string(),
    };
    Reply::Error(match mode {
        Mode::Exec => format!(
            "NOPERM ACLs rules changed between the moment the transaction was accumulated and the EXEC call. This command is no longer allowed for the following reason: {}",
            match d.reason {
                Reason::Command => "no permission to execute the command or subcommand",
                Reason::Key => "no permission to touch the specified keys",
                Reason::Channel => "no permission to access one of the channels used as arguments",
            }
        ),
        Mode::Script { .. } => format!("ERR ACL failure in script: {msg}"),
        _ => format!("NOPERM {msg}"),
    })
}

/// `ACL DRYRUN`'s verbose denial message.
pub fn dryrun_message(user: &str, full_name: &str, args: &[Bytes], d: Denied) -> String {
    match d.reason {
        Reason::Command => format!("User {user} has no permissions to run the '{full_name}' command"),
        Reason::Key => format!("User {user} has no permissions to access the '{}' key", String::from_utf8_lossy(&args[d.pos])),
        Reason::Channel => format!("User {user} has no permissions to access the '{}' channel", String::from_utf8_lossy(&args[d.pos])),
    }
}

/// Check (and log) a command the server layer executes itself (MONITOR, WAIT, SAVE…). `Err` is
/// the reply to send instead.
pub fn check_command(acl: &RwLock<Acl>, client: &Client, args: &[Bytes]) -> Result<(), Reply> {
    let (cmd, full) = crate::engine::Engine::resolve_as(args, true)?;
    match client.user.check(cmd, args) {
        Ok(()) => Ok(()),
        Err(d) => Err(deny(acl, client, &full, args, d, Mode::Top)),
    }
}

/// `ACL CAT <category>`: commands and subcommands in a category (`aclCatWithFlags`).
pub fn commands_in_category(bit: u32) -> Vec<&'static str> {
    // `aclCatWithFlags`: the command table's iteration order.
    let idx = index();
    let mut out = Vec::new();
    for e in crate::commands::order::commands() {
        for c in std::iter::once(e.cmd).chain(e.subs.iter().copied()) {
            if let Some(m) = idx.meta(c)
                && m.cats & bit != 0
            {
                out.push(m.full.as_str());
            }
        }
    }
    out
}

/// The full name (`config|get`) of a resolved command.
pub fn full_name(cmd: &Command) -> Option<&'static str> {
    index().meta(cmd).map(|m| m.full.as_str())
}

#[cfg(test)]
mod tests;
