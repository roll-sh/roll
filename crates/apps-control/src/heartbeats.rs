//! Heartbeats (Healthchecks', Cronitor's; docs/apps/README.md, "Heartbeats"): an environment's
//! `heartbeats` are things outside roll (a job on another machine, a backup, a device) that ping
//! `/__roll/heartbeat/<token>` on the environment's host at least every `every_s`. Each ping is
//! kept (`state`, so a takeover keeps it); one not come within `every_s` and `grace_s` of the
//! last (or of the heartbeat being added) is `heartbeat.missed` once, and the next ping is
//! `alert.resolved` for it. A token is shown once, in the answer that adds the heartbeat; only
//! its SHA-256 is kept.

use crate::model::{Heartbeat, HeartbeatState};
use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;

pub const PATH: &str = "/__roll/heartbeat/";

/// A new heartbeat's name and token, to be shown once.
pub(crate) type NewToken = (String, String);

fn hash(token: &str) -> String {
    Sha256::digest(token.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn ping_key(service: &str, env: &str, name: &str) -> String {
    format!("hb:{service}/{env}/{name}")
}

fn missed_key(service: &str, env: &str, name: &str) -> String {
    format!("hb-missed:{service}/{env}/{name}")
}

/// What's kept of a heartbeat's pings, gone (a new heartbeat by an old one's name).
pub(crate) fn forget(s: &crate::store::Store, service: &str, env: &str, name: &str) -> Result<()> {
    s.take_state(&ping_key(service, env, name))?;
    s.take_state(&missed_key(service, env, name))?;
    Ok(())
}

/// A heartbeat's due time: its last ping (or when it was added) and its period, before the grace.
fn due_at(h: &Heartbeat, last: Option<i64>) -> i64 {
    last.unwrap_or(h.added_at) + h.every_s as i64 * 1000
}

impl Apps {
    /// The heartbeats a PUT asks for, against the ones `current` has: one kept by name keeps its
    /// token and its start; a new one gets a token, returned (name, token) to be shown once.
    pub(crate) fn heartbeats_of(&self, v: &Value, current: &[Heartbeat]) -> Result<(Vec<Heartbeat>, Vec<NewToken>)> {
        let list = match v {
            Value::Null => return Ok((Vec::new(), Vec::new())),
            Value::Array(a) if a.len() <= crate::model::MAX_HEARTBEATS => a,
            _ => return Err(Error::BadRequest(format!("heartbeats is at most {} heartbeats, or null", crate::model::MAX_HEARTBEATS))),
        };
        let mut out: Vec<Heartbeat> = Vec::new();
        let mut tokens = Vec::new();
        for item in list {
            let o = item.as_object().ok_or_else(|| Error::BadRequest("heartbeats: each is {name, every_s, grace_s?}".into()))?;
            if let Some(k) = o.keys().find(|k| !["name", "every_s", "grace_s"].contains(&k.as_str())) {
                return Err(Error::BadRequest(format!("heartbeats: unknown field {k}")));
            }
            let name = o.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let every_s = o.get("every_s").and_then(Value::as_u64).and_then(|n| u32::try_from(n).ok()).ok_or_else(|| Error::BadRequest("heartbeats: every_s is a number of seconds".into()))?;
            let grace_s = match o.get("grace_s") {
                None | Some(Value::Null) => 300,
                Some(g) => g.as_u64().and_then(|n| u32::try_from(n).ok()).ok_or_else(|| Error::BadRequest("heartbeats: grace_s is a number of seconds".into()))?,
            };
            if out.iter().any(|h| h.name == name) {
                return Err(Error::BadRequest("heartbeats: names are unique".into()));
            }
            let h = match current.iter().find(|h| h.name == name) {
                Some(c) => Heartbeat { every_s, grace_s, ..c.clone() },
                None => {
                    let token = format!("hb_{}", crate::model::random(24));
                    let h = Heartbeat { name: name.clone(), every_s, grace_s, added_at: code_sql::now_ms(), token_hash: hash(&token) };
                    tokens.push((name, token));
                    h
                }
            };
            h.check()?;
            out.push(h);
        }
        Ok((out, tokens))
    }

    /// A ping to `/__roll/heartbeat/<token>` on an environment's host: kept, and a missed one
    /// resolved. False: no heartbeat of it has that token.
    pub(crate) async fn heartbeat_ping(self: &Arc<Self>, service: &str, env: &str, token: &str) -> bool {
        let (sid, e) = (service.to_string(), env.to_string());
        let Ok(row) = self.db(move |s| s.environment(&sid, &e)).await else { return false };
        let want = hash(token);
        let Some(h) = row.heartbeats.iter().find(|h| h.token_hash == want) else { return false };
        let now = code_sql::now_ms();
        let (pk, mk, name) = (ping_key(service, env, &h.name), missed_key(service, env, &h.name), h.name.clone());
        let v = now.to_string();
        // Kept, then the missed mark taken (one delete: one resolve, however many pings at once).
        let was_missed = self
            .db(move |s| {
                s.set_state(&pk, &v)?;
                s.take_state(&mk)
            })
            .await
            .unwrap_or(false);
        let was_down = self.status_count(crate::status_page::heartbeat_key(service, env, &name), true, true).await;
        if was_missed || was_down {
            self.status_incident(service, crate::status_page::heartbeat_key(service, env, &name), false).await;
        }
        if was_missed {
            let (sid, e, data) = (service.to_string(), env.to_string(), json!({ "alert": "heartbeat.missed", "heartbeat": name }));
            self.logs.sink(format!("env:{service}/{env}"), crate::logs::Stream::System).line(format!("heartbeat {name} pinged again"));
            if let Err(err) = self.db(move |s| s.add_event("alert.resolved", &sid, &e, None, &data)).await {
                tracing::warn!(service, env, error = %err, "apps: a heartbeat's alert.resolved wasn't recorded");
            }
        }
        true
    }

    /// Owned services' heartbeats past due and grace, each `heartbeat.missed` once.
    pub(crate) async fn heartbeats_pass(self: &Arc<Self>, services: &[crate::store::ServiceRow], now: i64) {
        let Ok(all_envs) = self.db(|s| s.all_environments()).await else { return };
        for svc in services.iter().filter(|s| self.owns(&s.id)) {
            for e in all_envs.iter().filter(|e| e.service == svc.id && !e.paused && !e.heartbeats.is_empty()) {
                for h in &e.heartbeats {
                    let (pk, mk) = (ping_key(&svc.id, &e.name, &h.name), missed_key(&svc.id, &e.name, &h.name));
                    let (h2, now_s) = (h.clone(), now.to_string());
                    // Marked by a claim (insert if absent): two nodes' passes (a takeover) say it once.
                    let missed = self
                        .db(move |s| {
                            let last: Option<i64> = s.get_state(&pk)?.and_then(|v| v.parse().ok());
                            if now <= due_at(&h2, last) + h2.grace_s as i64 * 1000 || !s.claim_state(&mk, &now_s)? {
                                return Ok(None);
                            }
                            Ok(Some(last))
                        })
                        .await
                        .ok()
                        .flatten();
                    if let Some(last) = missed {
                        self.status_count(crate::status_page::heartbeat_key(&svc.id, &e.name, &h.name), false, false).await;
                        self.status_incident(&svc.id, crate::status_page::heartbeat_key(&svc.id, &e.name, &h.name), true).await;
                        let data = json!({ "heartbeat": h.name, "last_ping_at": last, "every_s": h.every_s });
                        self.logs.sink(format!("env:{}/{}", svc.id, e.name), crate::logs::Stream::System).line(format!("heartbeat {} missed", h.name));
                        let (sid, en) = (svc.id.clone(), e.name.clone());
                        if let Err(err) = self.db(move |s| s.add_event("heartbeat.missed", &sid, &en, None, &data)).await {
                            tracing::warn!(service = %svc.id, env = %e.name, error = %err, "apps: a heartbeat's heartbeat.missed wasn't recorded");
                        }
                    }
                }
            }
        }
    }

    /// An environment's heartbeats as they stand.
    pub(crate) async fn heartbeat_states_of(self: &Arc<Self>, service: &str, env: &str, beats: &[crate::model::Heartbeat]) -> Vec<HeartbeatState> {
        let keys: Vec<(String, String, String, Heartbeat)> = beats.iter().map(|h| (h.name.clone(), ping_key(service, env, &h.name), missed_key(service, env, &h.name), h.clone())).collect();
        self.db(move |s| {
            keys.into_iter()
                .map(|(name, pk, mk, h)| {
                    let last: Option<i64> = s.get_state(&pk)?.and_then(|v| v.parse().ok());
                    Ok(HeartbeatState { name, last_ping_at: last, due_at: due_at(&h, last), missed: s.get_state(&mk)?.is_some() })
                })
                .collect()
        })
        .await
        .unwrap_or_default()
    }
}
