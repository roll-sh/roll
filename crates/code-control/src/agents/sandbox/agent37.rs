//! The agent37 backend: sandboxes are agent37 instances (gVisor sandboxes; their Sandbox API,
//! `https://api.agent37.com/v1/instances`), created from roll's workspace template — the image in
//! `deploy/agent/Dockerfile`, registered once by the operator — so an instance boots straight into
//! `roll agent up`. The instance's `env` carries the machine token and the owner's provider env
//! (write-only on agent37's side: it's never returned by their API); `metadata.roll_sandbox` names
//! roll's sandbox, so a create whose answer was lost is found again by listing.
//!
//! Every call has timeouts, follows no redirects and reads at most a bounded body. The API key is
//! sent as a bearer token and never logged, returned or put in an error.

use super::{BackendError, BackendState, SandboxBackend, SandboxSpec};
use async_trait::async_trait;
use reqwest::{Method, StatusCode};
use serde_json::{Value, json};
use std::time::Duration;

/// Bodies read from agent37 at most: one instance, or the list of them.
const MAX_BODY: usize = 1 << 20;
const MAX_LIST_BODY: usize = 16 << 20;

#[derive(Clone)]
pub struct Agent37Config {
    /// `ROLL_AGENT37_API_KEY` (`sk_live_…`).
    pub api_key: String,
    /// The Sandbox API (`https://api.agent37.com`; tests point it at a mock).
    pub url: String,
    /// The workspace template made from `deploy/agent/Dockerfile` (optionally `name@revision`).
    pub template: String,
    /// `us`: the only region that runs `default` instances.
    pub region: String,
    /// How long a call may take; a create waits until the instance runs, so it gets `create_timeout`.
    pub timeout: Duration,
    pub create_timeout: Duration,
}

impl Agent37Config {
    pub fn new(api_key: String) -> Agent37Config {
        Agent37Config {
            api_key,
            url: "https://api.agent37.com".into(),
            template: "roll-agent-sandbox".into(),
            region: "us".into(),
            timeout: Duration::from_secs(30),
            create_timeout: Duration::from_secs(300),
        }
    }
}

impl std::fmt::Debug for Agent37Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent37Config").field("url", &self.url).field("template", &self.template).field("region", &self.region).finish_non_exhaustive()
    }
}

pub struct Agent37 {
    config: Agent37Config,
    http: reqwest::Client,
}

/// What a call answered: the status and the JSON body (`Null` when empty or not JSON).
struct Answer {
    status: StatusCode,
    body: Value,
}

impl Answer {
    /// `error.code` of an agent37 error.
    fn code(&self) -> &str {
        self.body["error"]["code"].as_str().or_else(|| self.body["error"].as_str()).unwrap_or("")
    }

    /// A short description of an error answer: agent37's code and message (safe to show, says
    /// their documentation), never anything of ours.
    fn describe(&self) -> String {
        let message: String = self.body["error"]["message"].as_str().unwrap_or("").chars().take(300).collect();
        match (self.code(), message.is_empty()) {
            ("", _) => format!("agent37 answered {}", self.status),
            (code, true) => format!("agent37: {code}"),
            (code, false) => format!("agent37: {code}: {message}"),
        }
    }

    /// Worth retrying: capacity, a state in transition, rate limits, their side failing.
    fn transient(&self) -> bool {
        self.status.is_server_error() || self.status == StatusCode::TOO_MANY_REQUESTS || matches!(self.code(), "try_again" | "capacity_unavailable" | "no_capacity")
    }

    fn error(&self) -> BackendError {
        if self.status == StatusCode::UNAUTHORIZED {
            return BackendError::Permanent("agent37 refused roll's API key (ROLL_AGENT37_API_KEY)".into());
        }
        if self.transient() { BackendError::Transient(self.describe()) } else { BackendError::Permanent(self.describe()) }
    }
}

/// Instance ids are short lowercase alphanumerics; anything else isn't put in a path.
fn check_id(id: &str) -> Result<&str, BackendError> {
    if !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit()) {
        Ok(id)
    } else {
        Err(BackendError::Permanent(format!("{id:?} isn't an agent37 instance id")))
    }
}

impl Agent37 {
    pub fn new(config: Agent37Config) -> anyhow::Result<Agent37> {
        let http = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).redirect(reqwest::redirect::Policy::none()).build()?;
        Ok(Agent37 { config, http })
    }

    async fn call(&self, method: Method, path: &str, body: Option<&Value>, timeout: Duration, cap: usize) -> Result<Answer, BackendError> {
        let url = format!("{}{path}", self.config.url.trim_end_matches('/'));
        let mut req = self.http.request(method, &url).bearer_auth(&self.config.api_key).timeout(timeout).header("accept", "application/json");
        if let Some(b) = body {
            req = req.json(b);
        }
        // reqwest's errors name the URL, never headers: the key stays out of them.
        let mut resp = req.send().await.map_err(|e| BackendError::Transient(format!("agent37 is unreachable: {}", e.without_url())))?;
        let status = resp.status();
        let mut bytes = Vec::new();
        loop {
            match resp.chunk().await {
                Ok(Some(c)) => {
                    if bytes.len() + c.len() > cap {
                        return Err(BackendError::Transient(format!("agent37 answered more than {cap} bytes")));
                    }
                    bytes.extend_from_slice(&c);
                }
                Ok(None) => break,
                Err(e) => return Err(BackendError::Transient(format!("reading agent37's answer: {}", e.without_url()))),
            }
        }
        let body = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
        Ok(Answer { status, body })
    }

    async fn lifecycle(&self, id: &str, action: &str) -> Result<(), BackendError> {
        let a = self.call(Method::POST, &format!("/v1/instances/{}/{action}", check_id(id)?), None, self.config.timeout, MAX_BODY).await?;
        match a.status {
            s if s.is_success() => Ok(()),
            StatusCode::NOT_FOUND => Err(BackendError::Permanent(format!("agent37 has no instance {id}"))),
            // A lifecycle action in the wrong state (a stop while it's starting): the next look decides.
            StatusCode::BAD_REQUEST if a.code() == "invalid_request" => Err(BackendError::Transient(a.describe())),
            _ => Err(a.error()),
        }
    }
}

/// An agent37 status, as roll sees it.
pub fn state_of(instance: &Value) -> BackendState {
    match instance["status"].as_str().unwrap_or("") {
        "provisioning" => BackendState::Creating,
        "running" => BackendState::Running,
        // Sleeping releases compute like a stop (roll doesn't turn auto-sleep on, but a workspace
        // that fell past due is slept).
        "stopped" | "sleeping" => BackendState::Stopped,
        "stopping" | "starting" | "restarting" | "updating" | "waking" => BackendState::Busy,
        "failed" => {
            let why = instance["status_reason"]["message"].as_str().unwrap_or("the instance failed");
            BackendState::Failed(format!("agent37: {}", why.chars().take(300).collect::<String>()))
        }
        "deleting" | "deleted" => BackendState::Gone,
        other => BackendState::Failed(format!("agent37 reports an unknown status {other:?}")),
    }
}

#[async_trait]
impl SandboxBackend for Agent37 {
    fn name(&self) -> &'static str {
        "agent37"
    }

    async fn create(&self, spec: &SandboxSpec) -> Result<String, BackendError> {
        if let Some(id) = self.find(&spec.id).await? {
            return Ok(id);
        }
        let body = json!({
            "template": self.config.template,
            "resources": { "cpu": spec.size.cpu, "memory": spec.size.memory_gb, "disk": spec.size.disk_gb },
            "type": "default",
            "region": self.config.region,
            "name": format!("roll {}", spec.name),
            "metadata": { "roll_sandbox": spec.id },
            "env": spec.env,
            // Always on: a sleeping sandbox drops its daemon's link.
            "auto_sleep": false,
        });
        let a = self.call(Method::POST, "/v1/instances", Some(&body), self.config.create_timeout, MAX_BODY).await?;
        if !a.status.is_success() {
            return Err(a.error());
        }
        let id = a.body["id"].as_str().ok_or_else(|| BackendError::Transient("agent37's create answer has no instance id".into()))?;
        Ok(check_id(id)?.to_string())
    }

    async fn find(&self, sandbox_id: &str) -> Result<Option<String>, BackendError> {
        let a = self.call(Method::GET, "/v1/instances", None, self.config.timeout, MAX_LIST_BODY).await?;
        if !a.status.is_success() {
            return Err(a.error());
        }
        let found = a.body["data"].as_array().into_iter().flatten().find(|i| {
            i["metadata"]["roll_sandbox"].as_str() == Some(sandbox_id) && !matches!(i["status"].as_str(), Some("deleting" | "deleted"))
        });
        match found.and_then(|i| i["id"].as_str()) {
            Some(id) => Ok(Some(check_id(id)?.to_string())),
            None => Ok(None),
        }
    }

    async fn status(&self, backend_id: &str) -> Result<BackendState, BackendError> {
        let a = self.call(Method::GET, &format!("/v1/instances/{}", check_id(backend_id)?), None, self.config.timeout, MAX_BODY).await?;
        match a.status {
            s if s.is_success() => Ok(state_of(&a.body)),
            StatusCode::NOT_FOUND => Ok(BackendState::Gone),
            _ => Err(a.error()),
        }
    }

    async fn start(&self, backend_id: &str) -> Result<(), BackendError> {
        self.lifecycle(backend_id, "start").await
    }

    async fn stop(&self, backend_id: &str) -> Result<(), BackendError> {
        self.lifecycle(backend_id, "stop").await
    }

    async fn delete(&self, backend_id: &str) -> Result<(), BackendError> {
        let a = self.call(Method::DELETE, &format!("/v1/instances/{}", check_id(backend_id)?), None, self.config.timeout, MAX_BODY).await?;
        match a.status {
            // A repeat delete is a 404: it's gone either way.
            s if s.is_success() || s == StatusCode::NOT_FOUND => Ok(()),
            _ => Err(a.error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_map_to_roll_states() {
        let s = |v: &str| state_of(&json!({ "status": v, "status_reason": { "message": "boom" } }));
        assert_eq!(s("provisioning"), BackendState::Creating);
        assert_eq!(s("running"), BackendState::Running);
        assert_eq!(s("sleeping"), BackendState::Stopped);
        assert_eq!(s("starting"), BackendState::Busy);
        assert_eq!(s("failed"), BackendState::Failed("agent37: boom".into()));
        assert_eq!(s("deleted"), BackendState::Gone);
    }

    #[test]
    fn ids_are_checked_before_they_go_in_a_path() {
        assert!(check_id("ab12cd34ef").is_ok());
        for bad in ["", "../x", "AB", "a/b", "a?b"] {
            assert!(check_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn errors_say_whether_to_retry() {
        let a = |status: u16, code: &str| Answer { status: StatusCode::from_u16(status).unwrap(), body: json!({ "error": { "code": code, "message": "m" } }) };
        assert!(matches!(a(503, "no_capacity").error(), BackendError::Transient(_)));
        assert!(matches!(a(409, "try_again").error(), BackendError::Transient(_)));
        assert!(matches!(a(402, "insufficient_balance").error(), BackendError::Permanent(m) if m == "agent37: insufficient_balance: m"));
        assert!(matches!(a(401, "invalid_api_key").error(), BackendError::Permanent(m) if !m.contains("sk_")));
    }
}
