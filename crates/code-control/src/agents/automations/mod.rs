//! roll code automations (docs/code/automations.md): triggers, filters and steps that run the
//! owner's agents and talk to the world, durably.
//!
//! - **Inbox.** Every event a trigger sees is recorded in the owner's inbox (`automation_events`)
//!   with an idempotency key, in its own transaction (or the transaction of what caused it)
//!   before anything runs: [`Code::automation_event`], [`registry::record_event`].
//! - **Runs.** The engine matches each new event against the owner's enabled automations
//!   (trigger, then filters) and makes one run per automation and event in the same write that
//!   marks the event done. Runs and their steps are durable records with status, output, log
//!   lines and retries with backoff; they're claimed (a compare-and-swap with an expiry) by the
//!   node working on them, so a node that dies leaves them to be resumed and two nodes never
//!   work on one run at once.
//! - **Registry.** Triggers and steps are open ([`registry`]): integrations add theirs.
//! - **Acting.** Steps act through roll's API as the automation ([`StepContext::api`]: the
//!   commands route, the sandboxes route, landing), so wakes, grants, policy and cluster routing
//!   are the API's own, and daemon commands are idempotent by ids made from the run and step.

pub mod context;
pub mod filter;
pub mod registry;
pub(crate) mod store;
pub mod template;

pub use context::{ApiAnswer, HttpAnswer, HttpError, Secret, StepContext};
pub use registry::{NewEvent, OwnerRef, Registry, StepKind, StepOutcome, TriggerSource, record_event};
pub use store::SCHEMA;

use crate::review::apps::{PublicOnly, check_url, literal_private_host};
use crate::{Caller, Code, Error, Result};
use parking_lot::Mutex;

/// Call tokens (one API call of a step, as its automation) start with this.
pub const TOKEN_PREFIX: &str = "rlr_";

/// The engine's state on this node, and the registry.
#[derive(Default)]
pub struct Automations {
    /// Every trigger source and step kind.
    pub registry: Registry,
    /// The outgoing HTTP client (built on first use).
    http: Mutex<Option<reqwest::Client>>,
    /// Wakes the engine (a new event, a run to look at).
    pub(crate) wake: tokio::sync::Notify,
}

impl Code {
    /// Have the engine look at `owner_id`'s inbox and runs now.
    pub fn wake_automations(&self, _owner_id: &str) {
        self.automations.wake.notify_one();
    }

    /// The caller a step's call token stands for: the owner, as `automation:<id>` (or
    /// `agent:<run_as>`). Looked up on every request, like every token.
    pub(crate) fn automation_caller(&self, token: &str) -> Result<Caller> {
        let hash = crate::hash_token(token);
        let Some((owner_id, name)) = store::token(self.db.sql(), &hash)? else { return Err(Error::Unauthorized) };
        let Some(o) = self.db.owner_by_id(&owner_id)? else { return Err(Error::Unauthorized) };
        Ok(Caller::Owner { id: o.id, name: o.name, token: name })
    }

    fn automation_client(&self) -> std::result::Result<reqwest::Client, HttpError> {
        let mut c = self.automations.http.lock();
        if let Some(c) = c.as_ref() {
            return Ok(c.clone());
        }
        let mut b = reqwest::Client::builder().timeout(context::HTTP_TIMEOUT).redirect(reqwest::redirect::Policy::none()).user_agent("roll-automations");
        if !self.private_webhooks_allowed() {
            b = b.dns_resolver(PublicOnly).no_proxy();
        }
        let client = b.build().map_err(|e| HttpError::Failed(e.to_string()))?;
        *c = Some(client.clone());
        Ok(client)
    }

    /// [`StepContext::http`].
    pub(crate) async fn automation_http(&self, method: &str, url: &str, headers: &[(String, String)], body: Option<Vec<u8>>) -> std::result::Result<HttpAnswer, HttpError> {
        let u = check_url(url).map_err(|e| HttpError::Refused(e.to_string().replace("webhook_url", "url")))?;
        if !self.private_webhooks_allowed() && literal_private_host(&u) {
            return Err(HttpError::Refused(format!("{} is a private address", u.host_str().unwrap_or_default())));
        }
        let method = reqwest::Method::from_bytes(method.trim().to_ascii_uppercase().as_bytes()).map_err(|_| HttpError::Refused(format!("{method} isn't an HTTP method")))?;
        let client = self.automation_client()?;
        let mut req = client.request(method, u);
        for (k, v) in headers {
            req = req.header(k.as_str(), v.as_str());
        }
        if let Some(b) = body {
            req = req.body(b);
        }
        let mut resp = req.send().await.map_err(|e| HttpError::Failed(without_url(&e)))?;
        let status = resp.status().as_u16();
        let headers = resp.headers().iter().take(64).map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).chars().take(1024).collect())).collect();
        let (mut body, mut truncated) = (Vec::new(), false);
        while let Some(chunk) = resp.chunk().await.map_err(|e| HttpError::Failed(without_url(&e)))? {
            let room = context::MAX_BODY - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(HttpAnswer { status, headers, body, truncated })
    }
}

/// A request error without its URL (a URL may hold a token in its query).
fn without_url(e: &reqwest::Error) -> String {
    let e = e.to_string();
    match e.find(" for url (") {
        Some(i) => e[..i].to_string(),
        None => e,
    }
}
