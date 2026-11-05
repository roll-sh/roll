//! Linear (docs/code/automations.md, "Linear"): an owner's Linear workspace as a source of
//! automation events and a place automations act on.
//!
//! - **Connecting.** roll's OAuth app installed with `actor=app` (roll becomes a user of the
//!   workspace that can be assigned and mentioned: Linear's agents), or a personal API key.
//!   Tokens are sealed with `ROLL_SECRET_KEY`, refreshed before they expire (OAuth access tokens
//!   last a day), and never returned or logged ([`store`], [`client`]).
//! - **Webhooks** ([`webhook`]): `Linear-Signature` (HMAC-SHA256 of the body) and the body's
//!   `webhookTimestamp` within a minute; every delivery's events go into the owner's inbox with
//!   Linear's own ids as their keys, in one write, before the 200.
//! - **Triggers** ([`trigger`]): issues created or updated (team, project, labels, state,
//!   assignee, priority, text), comments (mentions of roll), and agent sessions (roll assigned
//!   or mentioned; a reply in the session).
//! - **Steps** ([`steps`]): comment, update an issue, link a change or PR, create an issue, post
//!   an agent activity; and `linear.agent_session` ([`session`]), which shows a roll thread's
//!   work in Linear as activities, turns its requests into elicitations and answers them from the
//!   user's reply, and steers the thread with follow-ups.

pub mod api;
pub(crate) mod client;
pub(crate) mod session;
pub(crate) mod steps;
pub(crate) mod store;
pub mod templates;
pub(crate) mod trigger;
pub mod webhook;

pub use store::SCHEMA;

use crate::Code;
use parking_lot::RwLock;
use std::sync::Arc;

/// The integration's `source` and the prefix of its step kinds.
pub const SOURCE: &str = "linear";

/// What OAuth asks for: reading and writing, and being an agent (assignable, mentionable).
pub const SCOPES: &str = "read,write,issues:create,comments:create,app:assignable,app:mentionable";

/// roll's Linear app, from the server's environment. Without a client id and secret owners can
/// still connect with a personal API key.
#[derive(Clone)]
pub struct LinearConfig {
    /// `ROLL_LINEAR_CLIENT_ID`.
    pub client_id: Option<String>,
    /// `ROLL_LINEAR_CLIENT_SECRET`.
    pub client_secret: Option<String>,
    /// `ROLL_LINEAR_WEBHOOK_SECRET`: the app's webhook signing secret (one for every workspace
    /// that installs it).
    pub webhook_secret: Option<String>,
    /// `ROLL_LINEAR_API_URL`: GraphQL at `{api_url}/graphql`, OAuth tokens at
    /// `{api_url}/oauth/token` (default `https://api.linear.app`).
    pub api_url: String,
    /// `ROLL_LINEAR_AUTHORIZE_URL` (default `https://linear.app/oauth/authorize`).
    pub authorize_url: String,
    /// `ROLL_LINEAR_REDIRECT_URI` (default `{roll's API URL}/v1/code/linear/oauth/callback`).
    pub redirect_uri: Option<String>,
}

impl Default for LinearConfig {
    fn default() -> Self {
        LinearConfig {
            client_id: None,
            client_secret: None,
            webhook_secret: None,
            api_url: "https://api.linear.app".into(),
            authorize_url: "https://linear.app/oauth/authorize".into(),
            redirect_uri: None,
        }
    }
}

impl std::fmt::Debug for LinearConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LinearConfig")
            .field("client_id", &self.client_id)
            .field("client_secret", &self.client_secret.as_ref().map(|_| "[redacted]"))
            .field("webhook_secret", &self.webhook_secret.as_ref().map(|_| "[redacted]"))
            .field("api_url", &self.api_url)
            .field("authorize_url", &self.authorize_url)
            .field("redirect_uri", &self.redirect_uri)
            .finish()
    }
}

impl LinearConfig {
    pub fn from_env() -> LinearConfig {
        Self::from_vars(|k| std::env::var(k).ok())
    }

    pub fn from_vars(get: impl Fn(&str) -> Option<String>) -> LinearConfig {
        let get = |k: &str| get(k).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let d = LinearConfig::default();
        LinearConfig {
            client_id: get("ROLL_LINEAR_CLIENT_ID"),
            client_secret: get("ROLL_LINEAR_CLIENT_SECRET"),
            webhook_secret: get("ROLL_LINEAR_WEBHOOK_SECRET"),
            api_url: get("ROLL_LINEAR_API_URL").map(|u| u.trim_end_matches('/').to_string()).unwrap_or(d.api_url),
            authorize_url: get("ROLL_LINEAR_AUTHORIZE_URL").unwrap_or(d.authorize_url),
            redirect_uri: get("ROLL_LINEAR_REDIRECT_URI"),
        }
    }

    /// The OAuth app, if both halves are set.
    pub fn oauth(&self) -> Option<(&str, &str)> {
        Some((self.client_id.as_deref()?, self.client_secret.as_deref()?))
    }
}

/// The integration's state on this node.
#[derive(Default)]
pub struct Linear {
    config: RwLock<LinearConfig>,
    pub(crate) http: client::Http,
    /// One refresh of an owner's OAuth token at a time on this node (other nodes are kept right
    /// by a compare-and-swap on the stored token).
    pub(crate) refreshing: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
}

impl Linear {
    pub fn config(&self) -> LinearConfig {
        self.config.read().clone()
    }
}

impl Code {
    /// Set roll's Linear app (`LinearConfig::from_env` in rolld; tests point it at a mock).
    pub fn configure_linear(&self, config: LinearConfig) {
        *self.linear.config.write() = config;
    }

    /// Where Linear sends an OAuth install back to.
    pub(crate) fn linear_redirect_uri(&self) -> String {
        self.linear.config().redirect_uri.unwrap_or_else(|| format!("{}/v1/code/linear/oauth/callback", self.config.api_url.trim_end_matches('/')))
    }
}

/// Register Linear's trigger source and step kinds (at `Code::open`).
pub fn register(code: &Arc<Code>) {
    let r = &code.automations.registry;
    r.register_trigger(Arc::new(trigger::LinearTrigger { code: Arc::downgrade(code) }));
    for s in steps::all(code) {
        r.register_step(s);
    }
    r.register_step(Arc::new(session::AgentSession { code: Arc::downgrade(code) }));
}
