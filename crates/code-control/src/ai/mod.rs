//! roll's own reviewer (docs/code/reviews.md, "roll's reviewer"), driving code-review:
//!
//! - **When:** a change opens, gets a new revision or is marked ready ([`Code::ai_on_change`]),
//!   or someone writes `@roll …` in a thread ([`Code::ai_on_comment`]). Each is a job in
//!   `ai_jobs`, run by [`Code::start_reviewer`] with bounded concurrency; jobs that were running
//!   when the daemon stopped start over.
//! - **What:** the change's diff since its merge base, or since roll's last review of it
//!   (incremental), minus ignored files; the mode (`auto` scores the change's risk); the
//!   repository context from the codebase index for `deep` and `max`.
//! - **With whose key:** the owner's own provider when they set one (bring your own key), else
//!   the operator's hosted provider. Spending caps apply before a hosted review starts.
//! - **Result:** a roll review (verdict, body, inline threads with suggestions) with an `ai`
//!   record of how it was made, and a metered usage row.

pub mod api;
mod index;
mod run;
pub mod store;

use crate::model::{Change, ChangeState, RepoRow, Visibility};
use crate::review::{Reviewer, ReviewerKind, Thread};
use crate::{Caller, Code, Error, Result, new_id};
use code_review::pricing::{KeySource, Rates};
use code_review::provider::{Anthropic, OpenAiCompatible, Provider, RetryPolicy};
use code_review::secret::Sealer;
use code_review::settings::ReviewerSettings;
use code_review::{Mode, commands};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub use code_review::pricing;

/// The operator's provider (`ROLL_REVIEW_PROVIDER` …).
#[derive(Clone, Debug)]
pub struct Hosted {
    /// `anthropic` or `openai_compatible`.
    pub provider: String,
    pub api_key: String,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub secondary_model: Option<String>,
}

#[derive(Clone, Debug)]
pub struct AiConfig {
    pub hosted: Option<Hosted>,
    /// Seals owners' own keys (`ROLL_SECRET_KEY`); without it bring-your-own-key is refused.
    pub secret_key: Option<String>,
    /// Reviews running at once.
    pub concurrency: usize,
    pub rates: Rates,
    /// Per-million-token rates of models roll doesn't know (`ROLL_REVIEW_MODEL_RATES`).
    pub model_rates: Vec<(String, (f64, f64, f64))>,
    /// Where an owner's Anthropic key is used (tests point it at a mock).
    pub anthropic_url: String,
    pub retry: RetryPolicy,
    /// Delay before retrying a failed review (then ×4), in milliseconds.
    pub retry_ms: i64,
    /// Build the codebase index (deep reviews, `/search`).
    pub index: bool,
}

impl Default for AiConfig {
    fn default() -> Self {
        AiConfig {
            hosted: None,
            secret_key: None,
            concurrency: 2,
            rates: Rates::default(),
            model_rates: Vec::new(),
            anthropic_url: "https://api.anthropic.com".into(),
            retry: RetryPolicy::default(),
            retry_ms: 30_000,
            index: true,
        }
    }
}

impl AiConfig {
    /// From the environment:
    /// - `ROLL_REVIEW_PROVIDER` (`anthropic` | `openai_compatible`; default `anthropic` when a key is set)
    /// - `ROLL_REVIEW_ANTHROPIC_API_KEY` or `ANTHROPIC_API_KEY`, `ROLL_REVIEW_MODEL`, `ROLL_REVIEW_SECONDARY_MODEL`
    /// - `ROLL_REVIEW_OPENAI_BASE_URL`, `ROLL_REVIEW_OPENAI_API_KEY`, `ROLL_REVIEW_OPENAI_MODEL`
    /// - `ROLL_SECRET_KEY`, `ROLL_REVIEW_CONCURRENCY`, `ROLL_REVIEW_PRICE_*`, `ROLL_REVIEW_MODEL_RATES`
    pub fn from_env() -> AiConfig {
        Self::from_lookup(|k| std::env::var(k).ok().filter(|v| !v.trim().is_empty()))
    }

    pub fn from_lookup(get: impl Fn(&str) -> Option<String>) -> AiConfig {
        let mut c = AiConfig { rates: Rates::from_lookup(&get), ..Default::default() };
        let provider = get("ROLL_REVIEW_PROVIDER").unwrap_or_else(|| "anthropic".into());
        c.hosted = match provider.as_str() {
            "openai_compatible" | "openai" => match (get("ROLL_REVIEW_OPENAI_BASE_URL"), get("ROLL_REVIEW_OPENAI_API_KEY")) {
                (Some(url), Some(key)) => Some(Hosted {
                    provider: "openai_compatible".into(),
                    api_key: key,
                    base_url: Some(url),
                    model: get("ROLL_REVIEW_OPENAI_MODEL").or_else(|| get("ROLL_REVIEW_MODEL")),
                    secondary_model: get("ROLL_REVIEW_SECONDARY_MODEL"),
                }),
                _ => None,
            },
            _ => get("ROLL_REVIEW_ANTHROPIC_API_KEY").or_else(|| get("ANTHROPIC_API_KEY")).map(|key| Hosted {
                provider: "anthropic".into(),
                api_key: key,
                base_url: get("ROLL_REVIEW_ANTHROPIC_BASE_URL"),
                model: get("ROLL_REVIEW_MODEL"),
                secondary_model: get("ROLL_REVIEW_SECONDARY_MODEL"),
            }),
        };
        c.secret_key = get("ROLL_SECRET_KEY");
        if let Some(n) = get("ROLL_REVIEW_CONCURRENCY").and_then(|v| v.parse().ok()) {
            c.concurrency = n;
        }
        if let Some(r) = get("ROLL_REVIEW_MODEL_RATES") {
            c.model_rates = pricing::parse_model_rates(&r);
        }
        c
    }
}

/// The reviewer's runtime state on [`Code`].
pub struct Ai {
    pub index: Option<code_index::Index>,
    pub(crate) sealer: Option<Sealer>,
    pub(crate) sealer_error: Option<String>,
    pub(crate) wake: tokio::sync::Notify,
    pub(crate) started: AtomicBool,
    pub(crate) index_dirty: parking_lot::Mutex<std::collections::BTreeSet<String>>,
    pub(crate) index_wake: tokio::sync::Notify,
    /// One index update at a time per repo.
    pub(crate) index_locks: parking_lot::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// First builds (a whole repo) run one at a time, so a few big repos can't add up in memory.
    pub(crate) first_builds: tokio::sync::Semaphore,
    /// First builds running in the background: done when the value turns true.
    pub(crate) building: parking_lot::Mutex<std::collections::HashMap<String, tokio::sync::watch::Receiver<bool>>>,
    /// Hosted spend reserved by reviews and chats still running, per owner: checked together
    /// with the month's metered spend, so concurrent jobs can't overrun a cap.
    pub(crate) reserved: parking_lot::Mutex<std::collections::HashMap<String, f64>>,
}

/// Spend held for a running job; released when dropped (after the job is metered).
pub(crate) struct Reservation<'a> {
    ai: &'a Ai,
    owner_id: String,
    usd: f64,
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        let mut m = self.ai.reserved.lock();
        if let Some(v) = m.get_mut(&self.owner_id) {
            *v -= self.usd;
            if *v <= 1e-9 {
                m.remove(&self.owner_id);
            }
        }
    }
}

impl Ai {
    /// Reserve `usd` of the owner's hosted spend if `spent` (metered this month, read by
    /// `spent`) plus what running jobs hold plus `usd` stays within `cap`. The check and the
    /// reservation happen under one lock.
    pub(crate) fn reserve(&self, owner_id: &str, cap: Option<f64>, usd: f64, spent: impl FnOnce() -> Result<f64>) -> Result<Option<Reservation<'_>>> {
        let mut m = self.reserved.lock();
        let held = m.get(owner_id).copied().unwrap_or(0.0);
        if let Some(cap) = cap
            && spent()? + held + usd > cap + 1e-9
        {
            return Ok(None);
        }
        *m.entry(owner_id.to_string()).or_insert(0.0) += usd;
        Ok(Some(Reservation { ai: self, owner_id: owner_id.to_string(), usd }))
    }

    /// Spend held by running jobs for this owner.
    pub fn reserved_usd(&self, owner_id: &str) -> f64 {
        self.reserved.lock().get(owner_id).copied().unwrap_or(0.0)
    }
}

impl Ai {
    pub fn new(config: &crate::Config) -> Ai {
        let (sealer, sealer_error) = match config.review.secret_key.as_deref().map(Sealer::new) {
            Some(Ok(s)) => (Some(s), None),
            Some(Err(e)) => (None, Some(e.to_string())),
            None => (None, None),
        };
        let index = if config.review.index {
            code_index::Index::open(&config.home.join("code").join("index")).map_err(|e| tracing::warn!(error = %e, "opening the codebase index")).ok()
        } else {
            None
        };
        Ai {
            index,
            sealer,
            sealer_error,
            wake: tokio::sync::Notify::new(),
            started: AtomicBool::new(false),
            index_dirty: parking_lot::Mutex::new(Default::default()),
            index_wake: tokio::sync::Notify::new(),
            index_locks: parking_lot::Mutex::new(Default::default()),
            first_builds: tokio::sync::Semaphore::new(1),
            building: parking_lot::Mutex::new(Default::default()),
            reserved: parking_lot::Mutex::new(Default::default()),
        }
    }
}

/// The provider a review runs on, and whose key it is.
pub(crate) struct Chosen {
    pub provider: Arc<dyn Provider>,
    pub key: KeySource,
    pub name: &'static str,
    pub secondary_model: Option<String>,
}

/// Is the change's author a person (not an agent)? Agents claim work items, or write as
/// `agent:<name>` / a GitHub App (`…[bot]`).
pub(crate) fn human_author(code: &Code, c: &Change) -> bool {
    let a = c.author.to_ascii_lowercase();
    if a.starts_with("agent:") || a.ends_with("[bot]") || a == "roll" {
        return false;
    }
    match c.work_item.as_deref().and_then(|w| code.db.work(w).ok().flatten()) {
        Some(w) => w.claimed_by.is_none(),
        None => true,
    }
}

pub(crate) fn month_of(t: i64) -> String {
    chrono::DateTime::from_timestamp(t, 0).map(|d| d.format("%Y-%m").to_string()).unwrap_or_else(|| "1970-01".into())
}

impl Code {
    // ── settings ──

    /// The reviewer settings in force for a repo: defaults, then the repo's settings, then
    /// `.roll/review.yaml`'s `reviewer:` section. `(effective, repo settings, file section, errors)`.
    pub async fn reviewer_settings(&self, r: &RepoRow) -> (ReviewerSettings, Value, Option<Value>, Vec<String>) {
        let mut s = ReviewerSettings::default();
        let stored = self.db.reviewer_settings(&r.id).ok().flatten().unwrap_or_else(|| json!({}));
        let mut errors = s.overlay(&stored);
        let cfg = self.review_config(r).await;
        let file = cfg.file.as_ref().and_then(|f| f.reviewer.clone());
        if let Some(f) = &file {
            errors.extend(s.overlay(f).into_iter().map(|e| format!(".roll/review.yaml: {e}")));
        }
        (s, stored, file, errors)
    }

    pub async fn patch_reviewer_settings(&self, r: &RepoRow, caller: &Caller, patch: &Value) -> Result<Value> {
        self.check_manage(caller, r)?;
        let mut stored = self.db.reviewer_settings(&r.id)?.unwrap_or_else(|| json!({}));
        let mut check = ReviewerSettings::default();
        check.overlay(&stored);
        let errs = check.overlay(patch);
        if !errs.is_empty() {
            return Err(Error::Invalid(errs.join("; ")));
        }
        if let (Some(o), Some(p)) = (stored.as_object_mut(), patch.as_object()) {
            for (k, v) in p {
                o.insert(k.clone(), v.clone());
            }
        }
        self.db.set_reviewer_settings(&r.id, &stored)?;
        self.db.audit(&r.id, &caller.identity().unwrap_or_default(), "reviewer_settings", patch)?;
        self.reviewer_view(r).await
    }

    /// `GET /repos/{o}/{r}/reviewer`.
    pub async fn reviewer_view(&self, r: &RepoRow) -> Result<Value> {
        let (s, stored, file, errors) = self.reviewer_settings(r).await;
        let (queued, running, last) = self.db.ai_job_summary(&r.id)?;
        let provider = self.choose_provider(&r.owner_id, false).ok().flatten();
        let index = match &self.ai.index {
            Some(i) => i.repo(&r.id).ok().map(|x| json!({ "head": x.head().ok().flatten(), "stats": x.stats().ok() })),
            None => None,
        };
        Ok(json!({
            "effective": s.to_value(),
            "settings": stored,
            "file": file,
            "errors": errors,
            "provider": provider.map(|p| json!({ "source": p.key.as_str(), "provider": p.name, "model": p.provider.model() })),
            "jobs": { "queued": queued, "running": running, "last": last },
            "index": index,
        }))
    }

    // ── providers and keys ──

    /// The owner's own provider, else the hosted one. `byok_only`: the owner's or nothing.
    pub(crate) fn choose_provider(&self, owner_id: &str, byok_only: bool) -> Result<Option<Chosen>> {
        let cfg = &self.config.review;
        if let Some(p) = self.db.owner_provider(owner_id)? {
            let sealer = self.ai.sealer.as_ref().ok_or_else(|| Error::Unprocessable("bring-your-own keys need ROLL_SECRET_KEY on the server".into()))?;
            let key = sealer.open(owner_id, &p.key_sealed).map_err(|e| Error::Unprocessable(e.to_string()))?;
            // IP literals never reach the resolver: check them on every use too.
            if !self.private_providers_allowed()
                && let Some(u) = p.base_url.as_deref().and_then(|u| url::Url::parse(u).ok())
                && crate::review::apps::literal_private_host(&u)
            {
                return Err(Error::Unprocessable("the owner's provider base_url is a private address".into()));
            }
            let (provider, name): (Arc<dyn Provider>, &'static str) = match p.provider.as_str() {
                "openai_compatible" => {
                    let o = OpenAiCompatible::new(p.base_url.as_deref().unwrap_or_default(), &key, p.model.as_deref().unwrap_or_default())
                        .map_err(|e| Error::Unprocessable(e.to_string()))?
                        .with_http(self.byok_http()?)
                        .with_retry(cfg.retry.clone());
                    (Arc::new(o), "openai_compatible")
                }
                _ => {
                    let a = Anthropic::new(&key, p.model.as_deref())
                        .with_base_url(p.base_url.as_deref().unwrap_or(&cfg.anthropic_url))
                        .with_http(self.byok_http()?)
                        .with_retry(cfg.retry.clone());
                    (Arc::new(a), "anthropic")
                }
            };
            let secondary = p.secondary_model.clone().or_else(|| (name == "anthropic").then(|| code_review::provider::anthropic::DEFAULT_SECONDARY_MODEL.to_string()));
            return Ok(Some(Chosen { provider, key: KeySource::Byok, name, secondary_model: secondary }));
        }
        if byok_only {
            return Ok(None);
        }
        let Some(h) = &cfg.hosted else { return Ok(None) };
        Ok(Some(hosted_provider(h, &cfg.retry).map_err(|e| Error::Unprocessable(e.to_string()))?))
    }

    pub fn set_owner_provider(&self, caller: &Caller, owner: &str, body: &Value) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::Forbidden(format!("not allowed to configure {}", o.name)));
        }
        let sealer = self.ai.sealer.as_ref().ok_or_else(|| {
            Error::Unprocessable(match &self.ai.sealer_error {
                Some(e) => format!("bring-your-own keys are unavailable: {e}"),
                None => "bring-your-own keys need ROLL_SECRET_KEY on the server".into(),
            })
        })?;
        let s = |k: &str| body.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty()).map(str::to_string);
        let provider = s("provider").unwrap_or_else(|| "anthropic".into());
        let key = s("api_key").ok_or_else(|| Error::Invalid("api_key is required".into()))?;
        if key.len() > 4096 {
            return Err(Error::Invalid("api_key is too long".into()));
        }
        let (base_url, model) = (s("base_url"), s("model"));
        // An owner's endpoint is the owner's to choose, but not this server's private network
        // (metadata, internal services): refused here when written, and on every connection.
        if let Some(u) = &base_url {
            let parsed = url::Url::parse(u).map_err(|e| Error::Invalid(format!("base_url: {e}")))?;
            if !parsed.username().is_empty() || parsed.password().is_some() {
                return Err(Error::Invalid("base_url can't carry credentials; set api_key".into()));
            }
            if !self.private_providers_allowed() && crate::review::apps::literal_private_host(&parsed) {
                return Err(Error::Invalid("base_url is a private address".into()));
            }
        }
        match provider.as_str() {
            "anthropic" => {
                if let Some(u) = &base_url
                    && !u.starts_with("https://")
                {
                    return Err(Error::Invalid("base_url must be https".into()));
                }
            }
            "openai_compatible" => {
                OpenAiCompatible::new(base_url.as_deref().unwrap_or_default(), &key, model.as_deref().unwrap_or_default()).map_err(|e| Error::Invalid(e.to_string()))?;
            }
            p => return Err(Error::Invalid(format!("provider is anthropic or openai_compatible, not {p}"))),
        }
        let row = store::OwnerProvider {
            provider,
            base_url,
            model,
            secondary_model: s("secondary_model"),
            key_sealed: sealer.seal(&o.id, &key),
            key_last4: code_review::secret::last4(&key),
            updated_at: crate::db::now(),
        };
        self.db.set_owner_provider(&o.id, Some(&row))?;
        self.owner_provider_view(caller, owner)
    }

    fn private_providers_allowed(&self) -> bool {
        self.config.allow_private_providers.unwrap_or(self.config.admin_token.is_none())
    }

    /// The HTTP client for owners' own providers: no redirects, and (unless private providers
    /// are allowed) names that resolve to private addresses are refused on every connection, so
    /// DNS can't be rebound around the check made when the key was set.
    fn byok_http(&self) -> Result<reqwest::Client> {
        let mut b = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none());
        if !self.private_providers_allowed() {
            b = b.dns_resolver(crate::review::apps::PublicOnly).no_proxy();
        }
        b.build().map_err(|e| Error::Internal(e.into()))
    }

    pub fn owner_provider_view(&self, caller: &Caller, owner: &str) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::NotFound(format!("review provider of {}", o.name)));
        }
        // No key of its own (hosted): the same shape with nulls, so the owner's own page doesn't
        // read a 404 (browsers log every one as an error). Outsiders still get the 404 above.
        let Some(p) = self.db.owner_provider(&o.id)? else {
            return Ok(json!({
                "owner": o.name, "provider": null, "base_url": null, "model": null,
                "secondary_model": null, "key_last4": null, "updated_at": null,
            }));
        };
        Ok(json!({
            "owner": o.name, "provider": p.provider, "base_url": p.base_url, "model": p.model,
            "secondary_model": p.secondary_model, "key_last4": p.key_last4, "updated_at": p.updated_at,
        }))
    }

    pub fn clear_owner_provider(&self, caller: &Caller, owner: &str) -> Result<()> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::Forbidden(format!("not allowed to configure {}", o.name)));
        }
        self.db.set_owner_provider(&o.id, None)?;
        Ok(())
    }

    // ── billing ──

    pub fn billing_view(&self, caller: &Caller, owner: &str) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::NotFound(format!("billing of {}", o.name)));
        }
        let b = self.db.billing(&o.id)?;
        let month = month_of(crate::db::now());
        let (spent, _, _) = self.db.usage_month(&o.id, &month, "")?;
        Ok(json!({
            "owner": o.name,
            "monthly_cap_usd": b.monthly_cap_usd,
            "on_cap": b.on_cap,
            "month": month,
            "spent_usd": round2(spent),
            "remaining_usd": b.monthly_cap_usd.map(|c| round2((c - spent).max(0.0))),
            "updated_at": b.updated_at,
        }))
    }

    pub fn set_billing(&self, caller: &Caller, owner: &str, body: &Value) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::Forbidden(format!("not allowed to configure {}", o.name)));
        }
        let cur = self.db.billing(&o.id)?;
        let cap = match body.get("monthly_cap_usd") {
            None => cur.monthly_cap_usd,
            Some(Value::Null) => None,
            Some(v) => match v.as_f64() {
                Some(x) if x.is_finite() && x >= 0.0 => Some(x),
                _ => return Err(Error::Invalid("monthly_cap_usd is a number ≥ 0 or null".into())),
            },
        };
        let on_cap = match body.get("on_cap") {
            None | Some(Value::Null) => cur.on_cap,
            Some(Value::String(s)) if ["pause", "byok_fallback", "standard_only"].contains(&s.as_str()) => s.clone(),
            Some(_) => return Err(Error::Invalid("on_cap is pause, byok_fallback or standard_only".into())),
        };
        self.db.set_billing(&o.id, cap, &on_cap)?;
        self.billing_view(caller, owner)
    }

    pub fn usage_view(&self, caller: &Caller, owner: &str, month: Option<&str>) -> Result<Value> {
        let o = self.owner_or_404(owner)?;
        if !Self::manages(caller, &o.id) {
            return Err(Error::NotFound(format!("usage of {}", o.name)));
        }
        let month = match month {
            Some(m) if m.len() == 7 && m.as_bytes()[4] == b'-' && m.chars().enumerate().all(|(i, c)| i == 4 || c.is_ascii_digit()) => m.to_string(),
            Some(m) => return Err(Error::Invalid(format!("month is YYYY-MM, not {m}"))),
            None => month_of(crate::db::now()),
        };
        let rows = self.db.usage_rows(&o.id, &month)?;
        let mut totals = json!({ "reviews": 0, "chats": 0, "price_usd": 0.0, "list_usd": 0.0, "provider_cost_usd": 0.0, "free_reviews": 0, "capped_reviews": 0,
            "tokens": { "input": 0, "output": 0, "cache_write": 0, "cache_read": 0 } });
        let mut by_repo: std::collections::BTreeMap<String, (u64, f64)> = Default::default();
        let mut by_mode: std::collections::BTreeMap<String, (u64, f64)> = Default::default();
        let mut by_key: std::collections::BTreeMap<String, (u64, f64)> = Default::default();
        let add = |v: &mut Value, k: &str, x: f64| v[k] = json!(v[k].as_f64().unwrap_or(0.0) + x);
        let inc = |v: &mut Value, k: &str, x: u64| v[k] = json!(v[k].as_u64().unwrap_or(0) + x);
        for u in &rows {
            inc(&mut totals, if u.kind == "chat" { "chats" } else { "reviews" }, 1);
            add(&mut totals, "price_usd", u.price_usd);
            add(&mut totals, "list_usd", u.list_usd);
            add(&mut totals, "provider_cost_usd", u.provider_cost_usd.unwrap_or(0.0));
            inc(&mut totals, "free_reviews", u.free as u64);
            inc(&mut totals, "capped_reviews", u.capped as u64);
            let t = &mut totals["tokens"];
            inc(t, "input", u.input_tokens);
            inc(t, "output", u.output_tokens);
            inc(t, "cache_write", u.cache_write_tokens);
            inc(t, "cache_read", u.cache_read_tokens);
            let e = by_repo.entry(u.repo_id.clone()).or_default();
            e.0 += 1;
            e.1 += u.price_usd;
            if u.kind == "review" {
                let e = by_mode.entry(u.mode.clone()).or_default();
                e.0 += 1;
                e.1 += u.price_usd;
            }
            let e = by_key.entry(u.key_source.clone()).or_default();
            e.0 += 1;
            e.1 += u.price_usd;
        }
        for k in ["price_usd", "list_usd", "provider_cost_usd"] {
            totals[k] = json!(round2(totals[k].as_f64().unwrap_or(0.0)));
        }
        let repos: Vec<Value> = by_repo
            .into_iter()
            .map(|(id, (n, p))| {
                let name = self.repo_by_id(&id).map(|r| r.full_name()).unwrap_or(id.clone());
                json!({ "repo": name, "repo_id": id, "count": n, "price_usd": round2(p) })
            })
            .collect();
        let modes: serde_json::Map<String, Value> = by_mode.into_iter().map(|(m, (n, p))| (m, json!({ "reviews": n, "price_usd": round2(p) }))).collect();
        let keys: serde_json::Map<String, Value> = by_key.into_iter().map(|(m, (n, p))| (m, json!({ "count": n, "price_usd": round2(p) }))).collect();
        let b = self.db.billing(&o.id)?;
        let spent = totals["price_usd"].as_f64().unwrap_or(0.0);
        Ok(json!({
            "owner": o.name,
            "month": month,
            "totals": totals,
            "by_repo": repos,
            "by_mode": modes,
            "by_key": keys,
            "cap": { "monthly_cap_usd": b.monthly_cap_usd, "on_cap": b.on_cap, "remaining_usd": b.monthly_cap_usd.map(|c| round2((c - spent).max(0.0))) },
            "recent": rows.iter().take(25).collect::<Vec<_>>(),
        }))
    }

    // ── triggers ──

    /// A change opened, got a new revision or was marked ready: review it if the repo's
    /// reviewer is on (and, when it's required, owe the change a review).
    pub(crate) fn ai_on_change(&self, r: &RepoRow, c: &Change) {
        let Some(me) = self.me.upgrade() else { return };
        let (r, c) = (r.clone(), c.clone());
        tokio::spawn(async move {
            if let Err(e) = me.ai_change(&r, &c).await {
                tracing::warn!(change = %c.id, error = %e, "scheduling roll's review");
            }
        });
    }

    async fn ai_change(&self, r: &RepoRow, c: &Change) -> Result<()> {
        if c.state != ChangeState::Open {
            return Ok(());
        }
        let (s, ..) = self.reviewer_settings(r).await;
        if !s.enabled {
            return Ok(());
        }
        if s.required {
            self.request_review(r, c, &Reviewer::roll(), true)?;
        }
        let eligible = s.auto_review && (!c.draft || s.drafts) && (c.trusted || c.run_approved) && !self.db.ai_paused(&c.id)?;
        if eligible {
            self.enqueue_review(r, c, None, false, None)?;
        }
        Ok(())
    }

    /// Queue a review of the change (one queued review per change: later revisions fold in).
    pub(crate) fn enqueue_review(&self, r: &RepoRow, c: &Change, mode: Option<Mode>, full: bool, by: Option<&str>) -> Result<store::Job> {
        if let Some(j) = self.db.queued_review_job(&c.id)?
            && mode.is_none()
            && !full
        {
            return Ok(j);
        }
        let id = new_id("aij");
        self.db.insert_ai_job(&id, &r.id, &c.id, "review", mode.map(|m| m.as_str()), full, None, by)?;
        self.ai.wake.notify_one();
        self.db.ai_job(&id)?.ok_or_else(|| Error::NotFound(format!("job {id}")))
    }

    /// `POST /changes/{id}/ai-review`.
    pub async fn request_ai_review(&self, caller: &Caller, r: &RepoRow, c: &Change, body: &Value) -> Result<store::Job> {
        self.check_write(caller, r)?;
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return Err(Error::Conflict(format!("change {} is {}", c.number, c.state.as_str())));
        }
        let mode = match body.get("mode").and_then(Value::as_str) {
            None | Some("auto") => None,
            Some(m) => Some(Mode::parse(m).ok_or_else(|| Error::Invalid("mode is auto, standard, deep or max".into()))?),
        };
        let full = body.get("full").and_then(Value::as_bool).unwrap_or(false);
        let (s, ..) = self.reviewer_settings(r).await;
        if s.required {
            self.request_review(r, c, &Reviewer::roll(), true)?;
        }
        self.enqueue_review(r, c, mode, full, caller.identity().as_deref())
    }

    /// A comment was written in a thread: run its `@roll` command, if any. Only writers can
    /// command roll (replies cost the owner money).
    pub(crate) fn ai_on_comment(&self, caller: &Caller, r: &RepoRow, c: &Change, th: &Thread, comment_id: &str) {
        let Some(cm) = th.comments.iter().find(|x| x.id == comment_id) else { return };
        if cm.author.kind == ReviewerKind::Roll {
            return;
        }
        let Some(cmd) = commands::parse(&cm.body) else { return };
        if self.check_write(caller, r).is_err() {
            tracing::debug!(change = %c.id, "ignoring @roll from someone without write access");
            return;
        }
        if let Err(e) = self.command_in_thread(r, c, th, &cm.author, cmd) {
            tracing::warn!(change = %c.id, error = %e, "@roll command");
        }
    }

    /// Run a command written in a thread and answer in it.
    pub(crate) fn command_in_thread(&self, r: &RepoRow, c: &Change, th: &Thread, by: &Reviewer, cmd: commands::Command) -> Result<()> {
        if let Some(reply) = self.run_command_now(r, c, Some(th), by, cmd)? {
            self.roll_reply(r, c, th, &reply)?;
        }
        Ok(())
    }

    fn roll_reply(&self, r: &RepoRow, c: &Change, th: &Thread, body: &str) -> Result<Thread> {
        self.reply(r, c, th, &Reviewer::roll(), body, &crate::review::Origin::ROLL)
    }

    /// Commands that need no model run right away and return roll's answer; the rest become
    /// jobs (which answer in their thread). `th` is `None` for a comment on a pull request's
    /// conversation (GitHub), where commands about particular lines can't run.
    pub(crate) fn run_command_now(&self, r: &RepoRow, c: &Change, th: Option<&Thread>, by: &Reviewer, cmd: commands::Command) -> Result<Option<String>> {
        use commands::Command::*;
        let reply = match cmd {
            Review { mode, full } => {
                self.db.set_ai_paused(&c.id, None)?;
                self.enqueue_review(r, c, mode, full, Some(&by.display()))?;
                let what = match (full, mode) {
                    (true, Some(m)) => format!("a full {} review", m.as_str()),
                    (true, None) => "a full review".into(),
                    (false, Some(m)) => format!("a {} review of what changed", m.as_str()),
                    (false, None) => "a review of what changed".into(),
                };
                format!("On it: {what}.")
            }
            Pause => {
                self.db.set_ai_paused(&c.id, Some(&by.display()))?;
                "Paused: I won't review new revisions of this change until someone says `@roll resume`.".into()
            }
            Resume => {
                self.db.set_ai_paused(&c.id, None)?;
                "Resumed: I'll review new revisions again.".into()
            }
            Resolve => {
                let mut n = 0;
                for t in self.db.threads(&c.id)? {
                    if !t.resolved && t.comments.first().is_some_and(|x| x.author.kind == ReviewerKind::Roll) {
                        self.set_resolved(r, c, &t, by, true)?;
                        n += 1;
                    }
                }
                format!("Resolved {n} of my threads on this change.")
            }
            Learn(text) => {
                let id = new_id("lrn");
                self.db.insert_learning(&id, &r.id, None, &text, "command", &by.display(), th.map(|t| t.id.as_str()))?;
                format!("Got it — I'll remember for future reviews: _{}_", text.trim())
            }
            Explain | Fix | Ask(_) => {
                let Some(th) = th else {
                    return Ok(Some("`@roll explain`, `@roll fix` and questions work in an inline comment on the lines you mean.".into()));
                };
                let (kind, extra) = match &cmd {
                    Explain => ("explain", Value::Null),
                    Fix => ("fix", Value::Null),
                    Ask(q) => ("ask", json!(q)),
                    _ => unreachable!(),
                };
                let id = new_id("aij");
                let command = json!({ "command": kind, "text": extra, "thread": th.id, "by": by });
                self.db.insert_ai_job(&id, &r.id, &c.id, "command", None, false, Some(&command), Some(&by.display()))?;
                self.ai.wake.notify_one();
                return Ok(None);
            }
        };
        Ok(Some(reply))
    }

    // ── learnings ──

    /// A thread roll opened was resolved: if the person explained why in a reply, remember it.
    pub(crate) fn ai_on_resolved(&self, r: &RepoRow, th: &Thread, by: &Reviewer) {
        if by.kind == ReviewerKind::Roll || !th.comments.first().is_some_and(|x| x.author.kind == ReviewerKind::Roll) {
            return;
        }
        if self.db.learning_for_thread(&th.id).unwrap_or(true) {
            return;
        }
        let Some(reason) = th.comments.iter().rev().find(|x| x.author.kind != ReviewerKind::Roll && commands::parse(&x.body).is_none()) else { return };
        let finding = th.comments[0].body.lines().next().unwrap_or_default().trim_start_matches("**").to_string();
        let text = format!("A finding like \"{}\" in {} was dismissed: {}", finding.replace("** ·", ":").trim(), th.path, reason.body.trim());
        let text: String = text.chars().take(1000).collect();
        if let Err(e) = self.db.insert_learning(&new_id("lrn"), &r.id, None, &text, "resolved", &by.display(), Some(&th.id)) {
            tracing::warn!(error = %e, "recording a learning");
        }
    }

    /// A roll review was dismissed with a reason: remember it.
    pub(crate) fn ai_on_dismissed(&self, r: &RepoRow, reviewer: &Reviewer, by: &str, reason: &str) {
        if reviewer.kind != ReviewerKind::Roll || reason.trim().is_empty() {
            return;
        }
        let text: String = format!("A review was dismissed: {}", reason.trim()).chars().take(1000).collect();
        if let Err(e) = self.db.insert_learning(&new_id("lrn"), &r.id, None, &text, "dismissed", by, None) {
            tracing::warn!(error = %e, "recording a learning");
        }
    }

    pub fn add_learning(&self, r: &RepoRow, caller: &Caller, body: &Value) -> Result<store::Learning> {
        self.check_write(caller, r)?;
        let text = body.get("text").and_then(Value::as_str).map(str::trim).filter(|t| !t.is_empty()).ok_or_else(|| Error::Invalid("text is required".into()))?;
        if text.len() > 2000 {
            return Err(Error::Invalid("text is longer than 2000 characters".into()));
        }
        let path = body.get("path").and_then(Value::as_str).map(str::trim).filter(|p| !p.is_empty());
        let id = new_id("lrn");
        self.db.insert_learning(&id, &r.id, path, text, "api", &caller.identity().unwrap_or_default(), None)?;
        self.db.learnings(&r.id)?.into_iter().find(|l| l.id == id).ok_or_else(|| Error::NotFound(id))
    }

    pub fn delete_learning(&self, r: &RepoRow, caller: &Caller, id: &str) -> Result<()> {
        self.check_write(caller, r)?;
        if !self.db.delete_learning(&r.id, id)? {
            return Err(Error::NotFound(format!("learning {id}")));
        }
        Ok(())
    }

    pub(crate) fn is_public(r: &RepoRow) -> bool {
        r.visibility == Visibility::Public
    }

    /// Start the reviewer: the job runner and the index updater. Idempotent.
    pub fn start_reviewer(self: &Arc<Self>) {
        if self.ai.started.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.db.requeue_running_ai_jobs() {
            Ok(n) if n > 0 => tracing::info!(jobs = n, "restarting reviews that were running"),
            Ok(_) => {}
            Err(e) => tracing::warn!(error = %e, "requeueing reviews"),
        }
        let me = self.clone();
        tokio::spawn(async move { me.run_jobs().await });
        if self.ai.index.is_some() {
            let me = self.clone();
            tokio::spawn(async move { me.run_indexer().await });
        }
    }
}

pub(crate) fn hosted_provider(h: &Hosted, retry: &RetryPolicy) -> anyhow::Result<Chosen> {
    Ok(match h.provider.as_str() {
        "openai_compatible" => {
            let o = OpenAiCompatible::new(h.base_url.as_deref().unwrap_or_default(), &h.api_key, h.model.as_deref().unwrap_or_default())?.with_retry(retry.clone());
            Chosen { provider: Arc::new(o), key: KeySource::Hosted, name: "openai_compatible", secondary_model: h.secondary_model.clone() }
        }
        _ => {
            let mut a = Anthropic::new(&h.api_key, h.model.as_deref()).with_retry(retry.clone());
            if let Some(u) = &h.base_url {
                a = a.with_base_url(u);
            }
            Chosen {
                provider: Arc::new(a),
                key: KeySource::Hosted,
                name: "anthropic",
                secondary_model: h.secondary_model.clone().or_else(|| Some(code_review::provider::anthropic::DEFAULT_SECONDARY_MODEL.to_string())),
            }
        }
    })
}

pub(crate) fn round2(x: f64) -> f64 {
    (x * 100.0).round() / 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_from_env() {
        let env = |pairs: &'static [(&'static str, &'static str)]| move |k: &str| pairs.iter().find(|(a, _)| *a == k).map(|(_, v)| v.to_string());
        let c = AiConfig::from_lookup(env(&[("ANTHROPIC_API_KEY", "sk-1"), ("ROLL_REVIEW_MODEL", "claude-sonnet-5-5"), ("ROLL_REVIEW_CONCURRENCY", "4")]));
        let h = c.hosted.unwrap();
        assert_eq!((h.provider.as_str(), h.api_key.as_str(), h.model.as_deref()), ("anthropic", "sk-1", Some("claude-sonnet-5-5")));
        assert_eq!(c.concurrency, 4);
        let o = AiConfig::from_lookup(env(&[
            ("ROLL_REVIEW_PROVIDER", "openai_compatible"),
            ("ROLL_REVIEW_OPENAI_BASE_URL", "https://openrouter.ai/api/v1"),
            ("ROLL_REVIEW_OPENAI_API_KEY", "sk-or"),
            ("ROLL_REVIEW_OPENAI_MODEL", "x/y"),
            ("ROLL_REVIEW_MODEL_RATES", "x/y=1/2/0.1"),
        ]));
        let h = o.hosted.unwrap();
        assert_eq!((h.provider.as_str(), h.model.as_deref()), ("openai_compatible", Some("x/y")));
        assert_eq!(o.model_rates[0].0, "x/y");
        assert!(AiConfig::from_lookup(|_| None).hosted.is_none());
        assert_eq!(month_of(0), "1970-01");
        assert_eq!(month_of(1_792_540_800), "2026-10");
    }
}
