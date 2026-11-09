//! Client keys: minted here, kept only as their SHA-256, looked up on every request.

use super::{Seen, Store, from_sql, sql_int};
use crate::{ClientRef, Secret};
use agent_proto::{GatewayClient, MachineId, gateway_limits};
use base64::Engine;
use rand::RngCore;
use rusqlite::{OptionalExtension, params};
use sha2::{Digest, Sha256};

/// A key's prefix (`rgk_`, a roll gateway key) and its length: `rgk_` + 43 base64url
/// characters (32 bytes). Anything else is refused before it's hashed.
const KEY_PREFIX: &str = "rgk_";
const KEY_LEN: usize = 4 + 43;
/// `last_used_at` is written at most this often per client.
const TOUCH_EVERY: u64 = 60_000;

fn hash(key: &str) -> String {
    hex::encode(Sha256::digest(key.as_bytes()))
}

pub(super) fn issue(store: &Store, name: &str, machine: Option<MachineId>, now: u64) -> anyhow::Result<(GatewayClient, Secret)> {
    let name = name.trim();
    if name.is_empty() || name.len() > gateway_limits::NAME || name.chars().any(char::is_control) {
        anyhow::bail!("a client's name is one line of 1 to {} bytes", gateway_limits::NAME);
    }
    let mut bytes = zeroize::Zeroizing::new([0u8; 32]);
    rand::rngs::OsRng.fill_bytes(bytes.as_mut());
    let key = Secret::new(format!("{KEY_PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(*bytes)));
    let mut id = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut id);
    let client = GatewayClient { id: format!("gwc_{}", hex::encode(id)), name: name.to_string(), machine, created_at: now };

    let mut db = store.db();
    let tx = db.transaction()?;
    let mut replaced = Vec::new();
    if let Some(m) = &client.machine {
        let mut q = tx.prepare("DELETE FROM clients WHERE machine = ?1 RETURNING id")?;
        replaced = q.query_map([m.as_str()], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
    }
    let live: i64 = tx.query_row("SELECT count(*) FROM clients", [], |r| r.get(0))?;
    if from_sql(live) >= gateway_limits::CLIENTS as u64 {
        anyhow::bail!("the gateway has {} clients already: revoke one first", gateway_limits::CLIENTS);
    }
    tx.execute(
        "INSERT INTO clients (id, name, machine, hash, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![client.id, client.name, client.machine.as_ref().map(MachineId::as_str), hash(key.expose()), sql_int(now)],
    )?;
    tx.commit()?;
    drop(db);
    let mut seen = store.seen();
    for id in replaced {
        seen.remove(&id);
    }
    Ok((client, key))
}

pub(super) fn verify(store: &Store, key: &str, now: u64) -> anyhow::Result<Option<ClientRef>> {
    if key.len() != KEY_LEN || !key.starts_with(KEY_PREFIX) {
        return Ok(None);
    }
    let db = store.db();
    let found = db
        .query_row("SELECT id, machine FROM clients WHERE hash = ?1", [hash(key)], |r| {
            Ok(ClientRef { id: r.get(0)?, machine: r.get::<_, Option<String>>(1)?.map(MachineId) })
        })
        .optional()?;
    let Some(client) = found else { return Ok(None) };
    let mut seen = store.seen();
    // The map holds live clients only (revoke and issue drop entries), so it's bounded by
    // `CLIENTS`; clear it if another process's revocations ever leave it oversized.
    if seen.len() > 4 * gateway_limits::CLIENTS {
        seen.clear();
    }
    let entry = seen.entry(client.id.clone()).or_insert(Seen { at: now, written: None });
    entry.at = entry.at.max(now);
    if entry.written.is_none_or(|w| now.saturating_sub(w) >= TOUCH_EVERY) {
        entry.written = Some(now);
        db.execute("UPDATE clients SET last_used_at = max(coalesce(last_used_at, 0), ?2) WHERE id = ?1", params![client.id, sql_int(now)])?;
    }
    Ok(Some(client))
}

pub(super) fn revoke(store: &Store, id: &str) -> anyhow::Result<bool> {
    let gone = store.db().execute("DELETE FROM clients WHERE id = ?1", [id])? > 0;
    store.seen().remove(id);
    Ok(gone)
}

pub(super) fn list(store: &Store) -> anyhow::Result<Vec<GatewayClient>> {
    let db = store.db();
    let mut q = db.prepare("SELECT id, name, machine, created_at FROM clients ORDER BY created_at, id")?;
    let rows = q.query_map([], |r| {
        Ok(GatewayClient { id: r.get(0)?, name: r.get(1)?, machine: r.get::<_, Option<String>>(2)?.map(MachineId), created_at: from_sql(r.get(3)?) })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub(super) fn last_used(store: &Store) -> anyhow::Result<Vec<(String, u64)>> {
    let db = store.db();
    let mut q = db.prepare("SELECT id, last_used_at FROM clients WHERE last_used_at IS NOT NULL ORDER BY id")?;
    let mut out: Vec<(String, u64)> = q.query_map([], |r| Ok((r.get(0)?, from_sql(r.get(1)?))))?.collect::<Result<_, _>>()?;
    // What `verify` saw since it last wrote: newer than the table.
    let seen = store.seen();
    for (id, at) in &mut out {
        if let Some(s) = seen.get(id.as_str()) {
            *at = (*at).max(s.at);
        }
    }
    Ok(out)
}
