//! roll's model gateway (docs/code/gateway.md): an HTTP server on one of the owner's machines
//! that takes the requests their coding agents make (Anthropic Messages, OpenAI Chat
//! Completions and Responses) and sends them on to the owner's upstreams (Anthropic, OpenAI,
//! Azure OpenAI, Bedrock, Vertex, OpenRouter, any compatible base URL), translating between the
//! APIs where an upstream speaks another one, failing over between upstreams on errors and
//! outages, and keeping usage and cost history.
//!
//! It runs inside `roll agent up` (`agent-host/src/gateway.rs` supervises it, and its
//! management is daemon commands, so there is no management HTTP API to protect), or alone as
//! `roll gateway serve --config <file>` ([`config`]).
//!
//! **What it never does** (docs/code/gateway.md, "Limits"): store, proxy or rotate a
//! consumer-subscription login (Claude.ai or ChatGPT sign-ins: upstreams take API keys and
//! cloud credentials only), present one client as another (requests go out as the gateway,
//! with the client's own headers dropped, never as Claude Code or Codex), or move between keys
//! of one provider to get around its rate limits (a 429 cools that upstream down and only an
//! upstream of another provider may take over).
//!
//! Layout:
//! - [`wire`]: the three APIs, the translation between them (requests, responses and streams)
//!   and a bounded SSE parser. No I/O.
//! - [`upstream`]: each upstream kind's URL, auth (bearer, `x-api-key`, `api-key`, SigV4, a
//!   Vertex access token) and stream framing (Bedrock's event stream).
//! - [`route`]: which upstreams serve a request, in order: routes, aliases, priorities,
//!   circuits, cool-downs and session affinity.
//! - [`net`]: the HTTP client: no redirects, private addresses refused unless allowed.
//! - [`server`]: the inbound routes, client keys, limits and the proxy loop.
//! - [`store`]: `gateway.db`: client keys (hashes), usage roll-ups, the request log.
//! - [`price`]: list prices and cost.
//! - [`config`]: the standalone config file and its hot reload.

pub mod config;
pub mod net;
pub mod price;
pub mod route;
pub mod server;
pub mod store;
pub mod upstream;
pub mod wire;

mod types;

pub use types::{ClientRef, Record, Secret, Usage};

use agent_proto::{GatewayClient, GatewayRun, GatewaySettings, GatewayStatus, LogBody, LogEntry, MachineId, UpstreamCheck, UsageBucket, UsageBy, UsageReport};
use std::collections::HashMap;
use std::path::Path;

/// A running (or stopped) gateway: its store, its settings and keys, and its listeners. Cheap
/// to clone; every clone is the same gateway.
#[derive(Clone)]
pub struct Gateway {
    // Filled in by `server` (the proxy helper owns this struct's internals).
    _private: (),
}

impl Gateway {
    /// Opens (or makes) `dir/gateway.db` (mode 0600). Doesn't listen: [`Gateway::apply`] does.
    pub async fn open(dir: &Path) -> anyhow::Result<Gateway> {
        let _ = dir;
        anyhow::bail!("model-gateway: not built yet")
    }

    /// Uses `settings` and `keys` (upstream id → key) from the next request on, and listens as
    /// they say: loopback plus `settings.listen`, on `settings.port`; nothing when disabled.
    /// Listeners are only rebound when the addresses or port change, so a change of upstreams
    /// or routes never drops a request in flight. Returns the run state it reached.
    pub async fn apply(&self, settings: GatewaySettings, keys: HashMap<String, Secret>) -> GatewayRun {
        let _ = (settings, keys);
        GatewayRun::Failed { error: "model-gateway: not built yet".into(), addresses: vec![] }
    }

    /// Whether it serves now.
    pub fn run_state(&self) -> GatewayRun {
        GatewayRun::Stopped
    }

    /// A receiver that sees every change of [`Gateway::run_state`] (a listener failing, a
    /// retry binding it).
    pub fn watch_run(&self) -> tokio::sync::watch::Receiver<GatewayRun> {
        tokio::sync::watch::channel(GatewayRun::Stopped).1
    }

    /// Stops listening (requests in flight finish).
    pub async fn stop(&self) {}

    /// Mints a client key (`rgk_…`). The store keeps only its hash; the caller seals the key to
    /// the machine that uses it and drops it. A machine's earlier client is revoked.
    pub fn issue_client(&self, name: &str, machine: Option<MachineId>) -> anyhow::Result<(GatewayClient, Secret)> {
        let _ = (name, machine);
        anyhow::bail!("model-gateway: not built yet")
    }

    /// Revokes a client: its next request is refused. `false`: no such client.
    pub fn revoke_client(&self, id: &str) -> anyhow::Result<bool> {
        let _ = id;
        anyhow::bail!("model-gateway: not built yet")
    }

    pub fn clients(&self) -> Vec<GatewayClient> {
        Vec::new()
    }

    /// Health of every upstream, when each client was last seen, the models offered.
    pub async fn status(&self) -> GatewayStatus {
        GatewayStatus { run: self.run_state(), upstreams: vec![], clients_seen: vec![], models: vec![] }
    }

    /// Asks one upstream (or every enabled one) for its models: never generates.
    pub async fn test(&self, upstream: Option<&str>) -> Vec<UpstreamCheck> {
        let _ = upstream;
        Vec::new()
    }

    pub fn usage(&self, from: u64, to: u64, by: UsageBy, bucket: UsageBucket) -> anyhow::Result<UsageReport> {
        let _ = (from, to, by, bucket);
        anyhow::bail!("model-gateway: not built yet")
    }

    pub fn log(&self, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> anyhow::Result<Vec<LogEntry>> {
        let _ = (after, before, limit, errors);
        anyhow::bail!("model-gateway: not built yet")
    }

    pub fn log_body(&self, seq: u64) -> anyhow::Result<Option<LogBody>> {
        let _ = seq;
        anyhow::bail!("model-gateway: not built yet")
    }

    /// Stops and deletes `gateway.db`: clients, usage and the log.
    pub async fn wipe(self) -> anyhow::Result<()> {
        anyhow::bail!("model-gateway: not built yet")
    }
}
