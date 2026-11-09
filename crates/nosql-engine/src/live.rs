//! What changes while a database is open: its settings ([`Database::set_settings`]), the
//! read-only flag ([`Database::set_read_only`], enforced by [`crate::hooks::ReadOnly`]) and the
//! recent client warnings ([`Database::recent_warnings`]).
//!
//! `Database::config().settings` are the settings the database opened with; statements read
//! [`Database::settings`], so thresholds (batch size, tombstones, restrictions per query),
//! guardrails, the authorizer and the auth cache validities apply to the next request. The
//! settings in [`RELOAD_SETTINGS`] are read when the database opens (the handshake's
//! authenticator, the shards drivers see, the guardrails other subsystems read at open), so
//! changing one of them needs a reload.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::{Mutex, RwLock};

use crate::config::Settings;
use crate::database::Database;

/// Warnings kept per database (oldest dropped first).
pub const RECENT_WARNINGS: usize = 100;

/// Characters of a statement kept with its warning.
pub const STATEMENT_CHARS: usize = 300;

/// Settings that only apply when the database opens.
pub const RELOAD_SETTINGS: &[&str] = &[
    "authenticator",
    "shards",
    "materialized_views_enabled",
    "sasi_indexes_enabled",
    "user_defined_functions_enabled",
];

/// A warning the database sent a client.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentWarning {
    /// Unix milliseconds.
    pub at_ms: i64,
    /// The statement (a batch's first one), cut at [`STATEMENT_CHARS`] characters.
    pub statement: String,
    /// The session's role (`None`: anonymous).
    pub role: Option<String>,
    pub warning: String,
}

/// The live state of a database.
pub(crate) struct Live {
    settings: RwLock<Arc<Settings>>,
    read_only: AtomicBool,
    warnings: Mutex<VecDeque<RecentWarning>>,
    /// Guardrail counters (`crate::guardrails::metrics`).
    pub(crate) metrics: crate::guardrails::Metrics,
    /// Per-row TTL expiration counters (`crate::rowttl`).
    pub(crate) expiration: crate::rowttl::Expiration,
    /// Tables whose ScyllaDB Paxos state table exists (`crate::describe::paxos`).
    pub(crate) paxos_tables: parking_lot::Mutex<std::collections::BTreeSet<[u8; 16]>>,
    /// The row cache as `MUTATION_FRAGMENTS()` reports it, per table dropped by REST
    /// `system/drop_sstable_caches` (`crate::mutation_fragments::RowCache`).
    pub(crate) row_cache: parking_lot::Mutex<std::collections::HashMap<[u8; 16], crate::mutation_fragments::Dropped>>,
    /// Whether the native transport (CQL) is on: ScyllaDB's REST
    /// `/storage_service/native_transport` stops and starts it.
    native_transport: tokio::sync::watch::Sender<bool>,
    /// Told about every option `UPDATE system.config` sets ([`Database::on_option_set`]).
    option_listeners: Mutex<Vec<OptionListener>>,
}

/// Hears `(option, value as system.config shows it)` after `UPDATE system.config` sets a live
/// option (Alternator follows its options this way).
pub type OptionListener = Arc<dyn Fn(&str, &str) + Send + Sync>;

impl Live {
    pub(crate) fn new(settings: Settings) -> Self {
        Live {
            settings: RwLock::new(Arc::new(settings)),
            read_only: AtomicBool::new(false),
            warnings: Mutex::new(VecDeque::new()),
            metrics: Default::default(),
            expiration: Default::default(),
            paxos_tables: Default::default(),
            row_cache: Default::default(),
            native_transport: tokio::sync::watch::channel(true).0,
            option_listeners: Mutex::new(Vec::new()),
        }
    }
}

impl Settings {
    /// The settings of [`RELOAD_SETTINGS`] that differ between `self` and `other`.
    pub fn needs_reload(&self, other: &Settings) -> Vec<&'static str> {
        let changed = [
            self.authenticator != other.authenticator,
            self.shards != other.shards,
            self.materialized_views_enabled != other.materialized_views_enabled,
            self.sasi_indexes_enabled != other.sasi_indexes_enabled,
            self.user_defined_functions_enabled != other.user_defined_functions_enabled,
        ];
        RELOAD_SETTINGS
            .iter()
            .zip(changed)
            .filter(|(_, c)| *c)
            .map(|(n, _)| *n)
            .collect()
    }
}

impl Database {
    /// Whether CQL clients are served (`nodetool enablebinary` / `disablebinary`).
    pub fn native_transport(&self) -> bool {
        *self.live.native_transport.borrow()
    }

    /// Stops (`false`: connections close and new ones are refused) or starts CQL service.
    pub fn set_native_transport(&self, on: bool) {
        self.live.native_transport.send_replace(on);
    }

    /// Follows [`Database::native_transport`] (the server closes its connections on `false`).
    pub fn native_transport_watch(&self) -> tokio::sync::watch::Receiver<bool> {
        self.live.native_transport.subscribe()
    }

    /// The current settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.live.settings.read().clone()
    }

    /// Applies new settings to the following requests, and drops the auth caches (their
    /// validities may have changed). When a setting of [`RELOAD_SETTINGS`] differs, nothing is
    /// applied and `Err` names them: reload the database with the new configuration.
    pub fn set_settings(&self, s: Settings) -> std::result::Result<(), Vec<&'static str>> {
        {
            let mut cur = self.live.settings.write();
            let reload = cur.needs_reload(&s);
            if !reload.is_empty() {
                return Err(reload);
            }
            *cur = Arc::new(s);
        }
        self.auth.caches.clear();
        Ok(())
    }

    /// Calls `f` after every live option `UPDATE system.config` sets.
    pub fn on_option_set(&self, f: OptionListener) {
        self.live.option_listeners.lock().push(f);
    }

    /// Shows option `name` in `system.config` as `value` (as the table shows it: strings
    /// JSON-quoted) without changing its source: the configuration of a component the engine
    /// doesn't own (Alternator's flags), so writing the shown value back restores it.
    pub fn show_option(&self, name: &str, value: &str) {
        let mut s = (*self.settings()).clone();
        if s.scylla.cql_sourced.contains(name) {
            return;
        }
        s.scylla.values.insert(name.to_string(), value.to_string());
        let _ = self.set_settings(s);
    }

    /// Tells the listeners that option `name` is now `value`.
    pub(crate) fn option_set(&self, name: &str, value: &str) {
        let listeners = self.live.option_listeners.lock().clone();
        for l in listeners {
            l(name, value);
        }
    }

    /// Rejects writes, schema, role and permission changes from now on (read-only branches);
    /// reads, `USE`, `LIST …` and `DESCRIBE` keep working. Internal writes (traces, the
    /// platform's password rotation) are not affected.
    pub fn set_read_only(&self, on: bool) {
        self.live.read_only.store(on, Ordering::Relaxed);
    }

    pub fn is_read_only(&self) -> bool {
        self.live.read_only.load(Ordering::Relaxed)
    }

    /// The last [`RECENT_WARNINGS`] warnings sent to clients, oldest first.
    pub fn recent_warnings(&self) -> Vec<RecentWarning> {
        self.live.warnings.lock().iter().cloned().collect()
    }

    /// Remembers the warnings of one request (`statement` is only built when there are some).
    pub(crate) fn record_warnings(
        &self,
        warnings: &[String],
        role: Option<&str>,
        statement: impl FnOnce() -> String,
    ) {
        if warnings.is_empty() {
            return;
        }
        let statement = truncate(&statement(), STATEMENT_CHARS);
        let at_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let mut ring = self.live.warnings.lock();
        for w in warnings {
            if ring.len() >= RECENT_WARNINGS {
                ring.pop_front();
            }
            ring.push_back(RecentWarning {
                at_ms,
                statement: statement.clone(),
                role: role.map(str::to_string),
                warning: w.clone(),
            });
        }
    }
}

/// `s` cut to `max` characters, with `…` when cut.
fn truncate(s: &str, max: usize) -> String {
    match s.char_indices().nth(max) {
        Some((at, _)) => format!("{}…", &s[..at]),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosql_types::Dialect;

    #[test]
    fn statements_are_cut_at_characters() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcd", 3), "abc…");
        assert_eq!(truncate("ééé€", 2), "éé…");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn reload_settings() {
        let a = Settings::defaults(Dialect::Cassandra);
        let mut b = a.clone();
        b.tombstone_warn_threshold = 1;
        b.authorizer = "CassandraAuthorizer".into();
        b.permissions_validity_ms = 0;
        assert!(a.needs_reload(&b).is_empty());
        b.shards = 2;
        b.authenticator = "PasswordAuthenticator".into();
        b.sasi_indexes_enabled = true;
        b.materialized_views_enabled = true;
        b.user_defined_functions_enabled = true;
        assert_eq!(a.needs_reload(&b), RELOAD_SETTINGS);
    }
}
