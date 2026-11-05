//! `gateway.db` (SQLite, mode 0600): client keys (only their SHA-256), usage roll-ups and the
//! request log. One connection behind a mutex; callers on the async side use
//! `spawn_blocking` or keep calls short (every statement here is indexed and small).
//!
//! - **Clients**: a key is `rgk_` + 32 random bytes (base64url); the row holds its SHA-256,
//!   never the key. [`Store::verify`] looks the hash up on every request (revocation is at
//!   once); it touches `last_used_at` at most once a minute per client.
//! - **Usage**: one row per (hour, client, machine, project, thread, model asked for, upstream,
//!   upstream model), summed on insert (`ON CONFLICT … DO UPDATE`), so a busy gateway writes a
//!   bounded number of rows. Days are sums of hours. Kept `retention_days`.
//! - **Log**: one row per request, the last 7 days and at most 100,000 rows; bodies (64 KiB
//!   each) in their own table, kept a day.
//!
//! The store helper owns this module; the signatures below are the contract with `server` and
//! the daemon.

use crate::{ClientRef, Record, Secret};
use agent_proto::{GatewayClient, LogBody, LogEntry, MachineId, ModelPrice, UsageBucket, UsageBy, UsageReport};
use std::path::Path;

pub struct Store {
    _private: (),
}

impl Store {
    pub fn open(path: &Path) -> anyhow::Result<Store> {
        let _ = path;
        anyhow::bail!("not built yet")
    }

    /// A store in memory (tests).
    pub fn memory() -> anyhow::Result<Store> {
        anyhow::bail!("not built yet")
    }

    /// Mints a client key; a client for the same machine is revoked first.
    pub fn issue(&self, name: &str, machine: Option<MachineId>, now: u64) -> anyhow::Result<(GatewayClient, Secret)> {
        let _ = (name, machine, now);
        anyhow::bail!("not built yet")
    }

    /// The client `key` belongs to, if it's a live key.
    pub fn verify(&self, key: &str, now: u64) -> anyhow::Result<Option<ClientRef>> {
        let _ = (key, now);
        anyhow::bail!("not built yet")
    }

    pub fn revoke(&self, id: &str) -> anyhow::Result<bool> {
        let _ = id;
        anyhow::bail!("not built yet")
    }

    pub fn clients(&self) -> anyhow::Result<Vec<GatewayClient>> {
        anyhow::bail!("not built yet")
    }

    /// (client id, last used) for every client that has been used.
    pub fn last_used(&self) -> anyhow::Result<Vec<(String, u64)>> {
        anyhow::bail!("not built yet")
    }

    /// Adds a finished request to the usage roll-ups and the log; its cost from `prices` (the
    /// owner's) over the built-in list ([`crate::price`]).
    pub fn record(&self, record: &Record, prices: &[ModelPrice], log_bodies: bool) -> anyhow::Result<()> {
        let _ = (record, prices, log_bodies);
        anyhow::bail!("not built yet")
    }

    pub fn usage(&self, from: u64, to: u64, by: UsageBy, bucket: UsageBucket) -> anyhow::Result<UsageReport> {
        let _ = (from, to, by, bucket);
        anyhow::bail!("not built yet")
    }

    pub fn log(&self, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> anyhow::Result<Vec<LogEntry>> {
        let _ = (after, before, limit, errors);
        anyhow::bail!("not built yet")
    }

    pub fn log_body(&self, seq: u64) -> anyhow::Result<Option<LogBody>> {
        let _ = seq;
        anyhow::bail!("not built yet")
    }

    /// Drops usage older than `retention_days`, the log older than 7 days, bodies older than a
    /// day.
    pub fn prune(&self, now: u64, retention_days: u16) -> anyhow::Result<()> {
        let _ = (now, retention_days);
        anyhow::bail!("not built yet")
    }
}
