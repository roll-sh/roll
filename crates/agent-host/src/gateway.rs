//! roll's model gateway on this machine (docs/code/gateway.md), both sides of it:
//!
//! - **Hosting it.** When the owner sets the gateway up here (`gateway.configure`), the daemon runs
//!   `crates/model-gateway` in its own process: [`Hosted`] holds the running [`Gateway`], its
//!   settings and its upstreams' keys. The settings and the keys are kept in `agent.db` (mode 0600)
//!   under [`SETTING`], the way endpoints keep theirs: a key arrives sealed to this daemon with
//!   the context [`upstream_key_context`] (so an endpoint's box never opens as an upstream's key, nor
//!   the other way round), or is read from 1Password on this machine (`key_ref`, memory only).
//!   The gateway's own data (client key hashes, usage, the request log) is `<home>/gateway/gateway.db`.
//!   Management is daemon commands only (`orchestrator/gateway.rs`): the gateway has no management
//!   HTTP API, so nothing can download a key.
//! - **Using it** (a member: any machine whose `roll-gateway` endpoint points at a gateway). Its
//!   client key is an ordinary endpoint key. Two things are the gateway's own: a fallback, when
//!   the owner turned it on, checks `<base>/health` ([`Health`]) as a provider's process starts
//!   and starts it on its own login when the gateway doesn't answer; and the provider's requests
//!   say which thread and project they are for ([`attribution_env`]) so usage is counted by them.

use crate::endpoints::Secret;
use agent_proto::{GatewayInfo, GatewaySettings, KeyState, UpstreamKey};
use model_gateway::Gateway;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

pub use agent_proto::gateway::upstream_key_context;

/// The store key of the hosted gateway's settings and upstream keys.
pub const SETTING: &str = "gateway";
/// How long a member waits for `<base>/health`.
pub const HEALTH_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a health answer is trusted.
pub const HEALTH_TTL: Duration = Duration::from_secs(30);
/// The headers the gateway counts usage by (`model-gateway`'s `Record.thread`/`project`).
pub const THREAD_HEADER: &str = "x-roll-thread";
pub const PROJECT_HEADER: &str = "x-roll-project";
/// The variables Codex reads them from (`model_providers.<id>.env_http_headers`): set per
/// session, so a process started for one thread names that thread.
pub const THREAD_ENV: &str = "ROLL_GATEWAY_THREAD";
pub const PROJECT_ENV: &str = "ROLL_GATEWAY_PROJECT";
/// Claude Code's extra request headers (`Name: Value` lines).
pub const CLAUDE_HEADERS_ENV: &str = "ANTHROPIC_CUSTOM_HEADERS";

/// The hosted gateway as `agent.db` keeps it.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct Stored {
    pub settings: GatewaySettings,
    /// Opened sealed keys, by upstream id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub keys: BTreeMap<String, Zeroizing<String>>,
}

impl std::fmt::Debug for Stored {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Stored").field("settings", &self.settings).field("keys", &self.keys.keys().collect::<Vec<_>>()).finish()
    }
}

/// A 1Password reference read for an upstream: the reference it was read from, and its value or
/// why it couldn't be read.
#[derive(Clone, Debug)]
pub struct Resolved {
    pub reference: String,
    pub value: Result<Secret, String>,
}

/// The gateway while this daemon hosts it.
#[derive(Clone)]
pub struct Hosted {
    pub gateway: Gateway,
    pub stored: Stored,
    /// `key_ref`s read, by upstream id (memory only).
    pub refs: BTreeMap<String, Resolved>,
    /// The task that says `hello` again whenever the run state changes.
    pub watcher: Option<tokio::task::AbortHandle>,
}

impl Hosted {
    /// The keys the gateway uses, by upstream: a reference's value, else the sealed one.
    pub fn keys(&self) -> HashMap<String, model_gateway::Secret> {
        let mut keys = HashMap::new();
        for u in &self.stored.settings.upstreams {
            let key = match &u.key_ref {
                Some(_) => self.refs.get(&u.id).and_then(|r| r.value.as_ref().ok()).map(|s| s.expose().to_string()),
                None => self.stored.keys.get(&u.id).map(|k| k.to_string()),
            };
            if let Some(k) = key {
                keys.insert(u.id.clone(), model_gateway::Secret::new(k));
            }
        }
        keys
    }

    /// What `hello` says about it (never a key). An upstream's 1Password reference that couldn't
    /// be read shows as that upstream's key state `none`, and in the log.
    pub fn info(&self) -> GatewayInfo {
        let keys = self
            .stored
            .settings
            .upstreams
            .iter()
            .map(|u| UpstreamKey {
                upstream: u.id.clone(),
                key: match (&u.key_ref, self.stored.keys.contains_key(&u.id)) {
                    (Some(_), _) if self.refs.get(&u.id).is_some_and(|r| r.value.is_ok()) => KeyState::Reference,
                    (Some(_), _) => KeyState::None,
                    (None, true) => KeyState::Sealed,
                    (None, false) => KeyState::None,
                },
            })
            .collect();
        GatewayInfo { settings: self.stored.settings.clone(), keys, clients: self.gateway.clients(), run: self.gateway.run_state() }
    }
}

/// Reads the settings' 1Password references, reusing what `previous` read for the same
/// reference (a reference is read again only when it changes or failed).
pub async fn resolve_refs(settings: &GatewaySettings, previous: &BTreeMap<String, Resolved>) -> BTreeMap<String, Resolved> {
    let mut refs = BTreeMap::new();
    for u in &settings.upstreams {
        let Some(reference) = &u.key_ref else { continue };
        let kept = previous.get(&u.id).filter(|r| &r.reference == reference && r.value.is_ok()).cloned();
        let resolved = match kept {
            Some(r) => r,
            None => {
                let value = crate::endpoints::read_reference(reference).await;
                if let Err(e) = &value {
                    tracing::warn!(upstream = %u.id, "the gateway upstream's key: {e}");
                }
                Resolved { reference: reference.clone(), value }
            }
        };
        refs.insert(u.id.clone(), resolved);
    }
    refs
}

/// The variables that make a provider's requests through a gateway say which thread and project
/// they are for: Claude Code's [`CLAUDE_HEADERS_ENV`] (added to any the provider's config already
/// sets), and [`THREAD_ENV`]/[`PROJECT_ENV`] for Codex's `env_http_headers`. `env` is the
/// provider's environment so far.
pub fn attribution_env(env: &[(String, String)], thread: &str, project: &str) -> Vec<(String, String)> {
    let mut headers: Vec<String> = env
        .iter()
        .rev()
        .find(|(k, _)| k == CLAUDE_HEADERS_ENV)
        .map(|(_, v)| {
            v.lines()
                .filter(|l| {
                    let name = l.split(':').next().unwrap_or_default().trim();
                    !l.trim().is_empty() && !name.eq_ignore_ascii_case(THREAD_HEADER) && !name.eq_ignore_ascii_case(PROJECT_HEADER)
                })
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    headers.push(format!("{THREAD_HEADER}: {thread}"));
    headers.push(format!("{PROJECT_HEADER}: {project}"));
    vec![
        (CLAUDE_HEADERS_ENV.to_string(), headers.join("\n")),
        (THREAD_ENV.to_string(), thread.to_string()),
        (PROJECT_ENV.to_string(), project.to_string()),
    ]
}

/// Whether gateways answer, as a member sees them: `GET <base>/health` (no key), at most
/// [`HEALTH_TIMEOUT`], trusted for [`HEALTH_TTL`]. Only endpoints with `fallback` ask.
#[derive(Default)]
pub struct Health {
    seen: std::sync::Mutex<HashMap<String, (Instant, bool)>>,
}

impl Health {
    /// The last answer said `base` is down (never asked: not down).
    pub fn known_down(&self, base: &str) -> bool {
        self.seen.lock().unwrap().get(base).is_some_and(|(_, up)| !up)
    }

    /// Asks `base` unless a fresh answer is known: (up, whether that differs from before; never
    /// asked counts as up).
    pub async fn check(&self, base: &str) -> (bool, bool) {
        let before = self.seen.lock().unwrap().get(base).copied();
        if let Some((at, up)) = before
            && at.elapsed() < HEALTH_TTL
        {
            return (up, false);
        }
        let up = health(base).await;
        let mut seen = self.seen.lock().unwrap();
        // One entry per gateway a machine's endpoints name (at most 32 endpoints).
        if seen.len() >= 64 {
            seen.clear();
        }
        seen.insert(base.to_string(), (Instant::now(), up));
        (up, before.map_or(!up, |(_, was)| was != up))
    }

    /// Forgets what is known about `base` (its endpoint was set again).
    pub fn forget(&self, base: &str) {
        self.seen.lock().unwrap().remove(base);
    }
}

async fn health(base: &str) -> bool {
    let Ok(client) = reqwest::Client::builder()
        .timeout(HEALTH_TIMEOUT)
        .connect_timeout(HEALTH_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("roll-agent/", env!("CARGO_PKG_VERSION")))
        .build()
    else {
        return false;
    };
    match client.get(format!("{base}/health")).send().await {
        Ok(r) => r.status().is_success(),
        Err(_) => false,
    }
}

/// What a provider's status says about the gateway it uses.
pub fn status_detail(endpoint_name: &str, down: bool) -> String {
    if down { "roll gateway offline: using its own login".to_string() } else { format!("via {endpoint_name}") }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_names_the_thread_and_keeps_other_headers() {
        let env = vec![(CLAUDE_HEADERS_ENV.to_string(), "x-team: a\nx-roll-thread: thr_old".to_string())];
        let got = attribution_env(&env, "thr_1", "prj_2");
        assert_eq!(
            got,
            vec![
                (CLAUDE_HEADERS_ENV.to_string(), "x-team: a\nx-roll-thread: thr_1\nx-roll-project: prj_2".to_string()),
                (THREAD_ENV.to_string(), "thr_1".to_string()),
                (PROJECT_ENV.to_string(), "prj_2".to_string()),
            ]
        );
        assert_eq!(attribution_env(&[], "t", "p")[0].1, "x-roll-thread: t\nx-roll-project: p");
    }

    #[test]
    fn stored_keys_never_show_in_debug() {
        let mut s = Stored::default();
        s.keys.insert("anthropic".into(), Zeroizing::new("sk-ant-secret".into()));
        assert!(!format!("{s:?}").contains("sk-ant-secret"));
    }

    #[tokio::test]
    async fn health_is_cached_and_says_when_it_changes() {
        let h = Health::default();
        // Nothing listens on port 1 of loopback.
        let base = format!("http://{}", std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, 1)));
        assert!(!h.known_down(&base));
        assert_eq!(h.check(&base).await, (false, true));
        assert!(h.known_down(&base));
        assert_eq!(h.check(&base).await, (false, false), "cached");
        h.forget(&base);
        assert!(!h.known_down(&base));
    }
}
