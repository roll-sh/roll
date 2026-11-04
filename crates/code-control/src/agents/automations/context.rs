//! What a step runs against ([`StepContext`]): the run's context and templates, the step's
//! config, the automation's sealed secrets, an SSRF-guarded HTTP client, the API as the
//! automation, durable scratch state and a log.

use super::registry::OwnerRef;
use super::template::{self, Secrets, Template};
use super::{TOKEN_PREFIX, store};
use crate::{Code, hash_token};
use agent_proto::automation::{AutomationId, AutomationRunId, LogLevel, LogLine, RunThread, Step, limits};
use agent_proto::{MachineId, ThreadId};
use axum::body::Body as AxumBody;
use axum::http::{Method, Request, StatusCode, header};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tower::ServiceExt;

/// Largest answer read from the API or an outside server.
pub const MAX_BODY: usize = 1 << 20;
/// How long an outgoing request may take, answer included.
pub const HTTP_TIMEOUT: Duration = Duration::from_secs(30);
/// How long a token minted for one API call works.
const TOKEN_TTL_MS: i64 = 10 * 60_000;

/// A secret's value. It never prints: `{:?}` shows `Secret([redacted])`.
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

/// One step attempt's view of its run. Built by the engine; steps only read it, log, keep state,
/// and call out through it.
pub struct StepContext {
    pub(crate) code: Arc<Code>,
    pub(crate) owner: OwnerRef,
    pub(crate) automation: AutomationId,
    pub(crate) automation_name: String,
    /// Runs act as `agent:<name>` when set.
    pub(crate) run_as: Option<String>,
    pub(crate) run: AutomationRunId,
    pub(crate) step: Step,
    pub(crate) attempt: u32,
    /// `{event, run, steps, automation, owner}`.
    pub(crate) ctx: Value,
    pub(crate) state: Value,
    pub(crate) log: Vec<LogLine>,
    pub(crate) threads: Vec<RunThread>,
    /// When the step's first attempt started (unix ms): waits count against its timeout.
    pub(crate) started_at: i64,
    router: OnceLock<axum::Router>,
}

/// One HTTP answer, read up to [`MAX_BODY`].
#[derive(Clone, Debug)]
pub struct HttpAnswer {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// More came than [`MAX_BODY`]; `body` is the start.
    pub truncated: bool,
}

impl HttpAnswer {
    /// The body as JSON when it parses, else as text.
    pub fn body_value(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&self.body).into_owned()))
    }
}

/// Why an outgoing request didn't get an answer.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum HttpError {
    /// The address is refused (private, not http(s), credentials in it): don't retry.
    #[error("{0}")]
    Refused(String),
    /// The network, a timeout, a refused connection: worth another attempt.
    #[error("{0}")]
    Failed(String),
}

/// What the API answered a call made as the automation.
#[derive(Clone, Debug)]
pub struct ApiAnswer {
    pub status: StatusCode,
    pub body: Value,
}

impl ApiAnswer {
    pub fn ok(&self) -> bool {
        self.status.is_success()
    }

    /// The API's error message, or the status.
    pub fn error(&self) -> String {
        self.body["error"].as_str().map(str::to_string).unwrap_or_else(|| format!("the API answered {}", self.status))
    }

    /// A 5xx, 429 or 408: the same call may work later.
    pub fn transient(&self) -> bool {
        self.status.is_server_error() || matches!(self.status, StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT)
    }
}

impl StepContext {
    /// A context for one attempt (the engine makes them; integrations' tests may too).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        code: Arc<Code>,
        owner: OwnerRef,
        automation: AutomationId,
        automation_name: String,
        run_as: Option<String>,
        run: AutomationRunId,
        step: Step,
        attempt: u32,
        ctx: Value,
        state: Value,
        started_at: i64,
    ) -> StepContext {
        StepContext { code, owner, automation, automation_name, run_as, run, step, attempt, ctx, state, log: vec![], threads: vec![], started_at, router: OnceLock::new() }
    }

    pub fn owner(&self) -> &OwnerRef {
        &self.owner
    }

    pub fn automation(&self) -> &AutomationId {
        &self.automation
    }

    pub fn automation_name(&self) -> &str {
        &self.automation_name
    }

    pub fn run(&self) -> &AutomationRunId {
        &self.run
    }

    /// The step as the automation defines it.
    pub fn step(&self) -> &Step {
        &self.step
    }

    /// 1 for the first attempt.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// The same for every attempt of this step in this run, on every node: `<run>.<step>`. Use
    /// it to make side effects happen once (a daemon command's id, an `Idempotency-Key`).
    pub fn idempotency_key(&self) -> String {
        format!("{}.{}", self.run, self.step.id)
    }

    /// A daemon command id for this step (`part` tells this step's commands apart).
    pub fn command_id(&self, part: &str) -> String {
        format!("auto_{}_{}_{part}", self.run.as_str().trim_start_matches(AutomationRunId::PREFIX), self.step.id)
    }

    /// The run's context: `event`, `run`, `steps` (earlier steps' outputs), `automation`,
    /// `owner`.
    pub fn context(&self) -> &Value {
        &self.ctx
    }

    /// When the step's first attempt started (unix ms).
    pub fn started_at(&self) -> i64 {
        self.started_at
    }

    /// The step's config, as written (its strings are templates: render the ones that are with
    /// [`render`](Self::render)).
    pub fn config<T: DeserializeOwned>(&self) -> Result<T, String> {
        let v = if self.step.config.is_null() { json!({}) } else { self.step.config.clone() };
        serde_json::from_value(v).map_err(|e| format!("{} config: {e}", self.step.kind))
    }

    /// A template filled in from the run's context (no secrets).
    pub fn render(&self, src: &str) -> Result<String, String> {
        template::render(src, &self.ctx).map_err(|e| e.to_string())
    }

    /// Every string of `v` filled in (no secrets); a string that is one expression keeps its
    /// value's type.
    pub fn render_json(&self, v: &Value) -> Result<Value, String> {
        template::render_json(v, &self.ctx, Secrets::Refused).map_err(|e| e.to_string())
    }

    /// A template that may read `{{secrets.<name>}}` (only where the kind's
    /// [`secrets_at`](super::StepKind::secrets_at) allows it: the check at save time refuses
    /// it elsewhere). Never log what it returns.
    pub fn render_secret(&self, src: &str) -> Result<Secret, String> {
        let t = Template::parse(src).map_err(|e| e.to_string())?;
        let get = |name: &str| self.secret(name).map(|s| s.0);
        t.render(&self.ctx, Secrets::Allowed(&get)).map(Secret).map_err(|e| e.to_string())
    }

    /// One of the automation's secrets, opened (`ROLL_SECRET_KEY`). `None` if there's no such
    /// secret or the server can't open it.
    pub fn secret(&self, name: &str) -> Option<Secret> {
        let sealed = store::secret(self.code.db.sql(), self.automation.as_str(), name).ok().flatten()?;
        let sealer = self.code.ai.sealer.as_ref()?;
        sealer.open(&store::secret_scope(self.automation.as_str(), name), &sealed).ok().map(Secret)
    }

    /// A line in the step's log (the newest [`limits::LOG_LINES`] are kept). Never log a
    /// secret or a token.
    pub fn log(&mut self, level: LogLevel, text: impl Into<String>) {
        let text: String = text.into().chars().take(limits::LOG_LINE).collect();
        self.log.push(LogLine { at: code_sql::now_ms().max(0) as u64, level, text });
        if self.log.len() > limits::LOG_LINES {
            self.log.remove(0);
        }
    }

    /// What this step kept between attempts and waits (null at first).
    pub fn state(&self) -> &Value {
        &self.state
    }

    /// Keep `v` for the step's next attempt or wait (stored with the step's outcome).
    pub fn set_state(&mut self, v: Value) {
        self.state = v;
    }

    /// The step started this thread: the run lists it (the canvas tags it with the
    /// automation) and its events wake the run.
    pub fn started_thread(&mut self, machine: &MachineId, thread: &ThreadId) {
        if !self.threads.iter().any(|t| &t.thread == thread) {
            self.threads.push(RunThread { machine: machine.clone(), thread: thread.clone(), step: self.step.id.clone() });
        }
    }

    /// An HTTP request to a public address: http(s) only, no credentials in the URL, private
    /// and loopback addresses refused (also when a name resolves to one, through
    /// `review::apps::PublicOnly`) unless the server allows private webhooks
    /// (`ROLL_CODE_WEBHOOK_ALLOW_PRIVATE`), no redirects, [`HTTP_TIMEOUT`], and the answer read
    /// up to [`MAX_BODY`].
    pub async fn http(&self, method: &str, url: &str, headers: &[(String, String)], body: Option<Vec<u8>>) -> Result<HttpAnswer, HttpError> {
        self.code.automation_http(method, url, headers, body).await
    }

    /// Call roll's API as the automation (as the owner, or as `agent:<run_as>`), routed like any
    /// request (in a cluster, to the node that serves it). The token made for the call works for
    /// this call only and is looked up like every token.
    pub async fn api(&self, method: Method, path: &str, body: Option<Value>) -> ApiAnswer {
        let name = match &self.run_as {
            Some(a) => format!("agent:{a}"),
            None => format!("automation:{}", self.automation),
        };
        let token = format!("{TOKEN_PREFIX}{}", crate::agents::api::random_hex(32));
        let hash = hash_token(&token);
        let expires = code_sql::now_ms() + TOKEN_TTL_MS;
        if let Err(e) = store::insert_token(self.code.db.sql(), &hash, &self.owner.id, self.automation.as_str(), &name, expires) {
            return ApiAnswer { status: StatusCode::SERVICE_UNAVAILABLE, body: json!({ "error": format!("recording the run's token: {e}") }) };
        }
        let router = self.router.get_or_init(|| crate::api::router(self.code.clone())).clone();
        let mut b = Request::builder().method(method).uri(path).header(header::AUTHORIZATION, format!("Bearer {token}"));
        let req = match body {
            Some(v) => {
                b = b.header(header::CONTENT_TYPE, "application/json");
                b.body(AxumBody::from(v.to_string()))
            }
            None => b.body(AxumBody::empty()),
        };
        let answer = match req {
            Ok(req) => match router.oneshot(req).await {
                Ok(resp) => {
                    let status = resp.status();
                    let bytes = axum::body::to_bytes(resp.into_body(), 16 << 20).await.unwrap_or_default();
                    ApiAnswer { status, body: serde_json::from_slice(&bytes).unwrap_or(Value::Null) }
                }
                Err(e) => match e {},
            },
            Err(e) => ApiAnswer { status: StatusCode::BAD_REQUEST, body: json!({ "error": e.to_string() }) },
        };
        if let Err(e) = store::delete_token(self.code.db.sql(), &hash) {
            tracing::warn!(error = %e, "dropping an automation's call token (it expires by itself)");
        }
        answer
    }
}
