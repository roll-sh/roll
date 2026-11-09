//! Following an agent thread in a chat (the `*.status` steps): one status message that changes
//! as the thread works (running, its steps, waiting on you, done with the change's link), a card
//! with buttons for each request it waits on, and the chat thread bound to the agent thread so
//! replies steer it and clicks answer it.
//!
//! The step waits without holding anything: thread records wake the run
//! (docs/code/automations.md, "Waiting"), and each wake reads the mirror, edits what changed and
//! waits again, until the thread's work ends. Everything it posts is recorded before the next
//! wake (`chat_marks`, `chat_requests`), so an attempt that runs again edits instead of
//! posting twice.

use super::cards::{self, Ended};
use super::{Binding, Install};
use crate::agents::automations::{StepContext, StepOutcome};
use code_chat::card::{RequestCard, StatusCard};
use serde::Deserialize;
use serde_json::{Value, json};
use std::time::Duration;

/// A platform's way of posting and editing cards.
#[async_trait::async_trait]
pub(crate) trait Surface: Send + Sync {
    /// Post the status card in `channel` (in `thread`, if set): the message's id.
    async fn post_status(&self, card: &StatusCard, channel: &str, thread: Option<&str>) -> Result<String, Failure>;
    async fn update_status(&self, card: &StatusCard, channel: &str, ts: &str) -> Result<(), Failure>;
    async fn post_request(&self, card: &RequestCard, channel: &str, thread: &str) -> Result<String, Failure>;
    async fn update_request(&self, card: &RequestCard, channel: &str, ts: &str) -> Result<(), Failure>;
    /// For Discord, the status card carries a Reply button naming the binding.
    fn with_binding(&self, _card: &mut StatusCard, _binding: &str) {}
}

/// Why a platform call failed, and whether trying later may work.
#[derive(Debug)]
pub(crate) struct Failure {
    pub transient: bool,
    pub message: String,
}

/// Where and what to follow.
#[derive(Clone, Debug, Default, Deserialize)]
pub(crate) struct Follow {
    pub machine: String,
    pub thread: String,
    pub channel: String,
    /// Post into this chat thread; unset: the status message starts one.
    #[serde(default)]
    pub thread_ts: Option<String>,
    /// Edit this message instead of posting one (a status posted by an earlier step).
    #[serde(default)]
    pub ts: Option<String>,
    /// Stop once the status and pending requests are posted (notifications), instead of following
    /// to the end.
    #[serde(default)]
    pub once: bool,
}

#[derive(Clone, Debug, Default, serde::Serialize, Deserialize)]
struct State {
    binding: Option<String>,
    thread_ts: Option<String>,
    ts: Option<String>,
    hash: Option<String>,
}

/// How long to wait for a thread the daemon hasn't reported yet.
const SHOWS_UP: i64 = 10 * 60_000;
/// Waits between wakes when nothing wakes the run sooner.
const IDLE: Duration = Duration::from_secs(120);

fn hash(v: &impl serde::Serialize) -> String {
    crate::hash_token(&serde_json::to_string(v).unwrap_or_default())
}

/// A transient platform failure keeps following (the next wake tries again); a permanent one
/// fails the step.
fn on_failure(ctx: &mut StepContext, f: Failure) -> StepOutcome {
    if f.transient {
        ctx.log(agent_proto::automation::LogLevel::Warn, format!("will try again: {}", f.message));
        StepOutcome::Wait { after: Duration::from_secs(30) }
    } else {
        StepOutcome::Fail(f.message)
    }
}

/// One wake of a following step.
pub(crate) async fn step(ctx: &mut StepContext, surface: &dyn Surface, install: &Install, f: &Follow) -> StepOutcome {
    let code = ctx.code.clone();
    let owner = ctx.owner().clone();
    if f.machine.is_empty() || f.thread.is_empty() || f.channel.is_empty() {
        return StepOutcome::Fail("machine, thread and channel are needed".into());
    }
    let mut st: State = serde_json::from_value(ctx.state().clone()).unwrap_or_default();
    let v = match cards::view(&code, &owner.id, &f.machine, &f.thread) {
        Ok(Some(v)) => v,
        Ok(None) if code_sql::now_ms() - ctx.started_at() < SHOWS_UP => return StepOutcome::Wait { after: Duration::from_secs(5) },
        Ok(None) => return StepOutcome::Fail(format!("thread {} never showed up on machine {}", f.thread, f.machine)),
        Err(e) => return StepOutcome::Retry(e.to_string()),
    };
    let mut card = cards::status(&code.config.web_url, &owner.name, &v);

    // The status message, once.
    let ts_mark = format!("status:{}", ctx.idempotency_key());
    if st.ts.is_none() {
        st.ts = f.ts.clone().filter(|t| !t.is_empty()).or_else(|| super::mark(&code, &ts_mark));
    }
    let ts = match st.ts.clone() {
        Some(ts) => ts,
        None => match surface.post_status(&card, &f.channel, f.thread_ts.as_deref()).await {
            Ok(ts) => {
                super::set_mark(&code, &ts_mark, &ts);
                st.ts = Some(ts.clone());
                st.hash = None;
                ts
            }
            Err(e) => return on_failure(ctx, e),
        },
    };
    let thread_ts = st.thread_ts.clone().or_else(|| f.thread_ts.clone().filter(|t| !t.is_empty())).unwrap_or_else(|| ts.clone());
    st.thread_ts = Some(thread_ts.clone());
    let binding: Binding = match &st.binding {
        Some(id) => match super::binding_by_id(&code, id) {
            Ok(Some(b)) => b,
            Ok(None) | Err(_) => match bind(ctx, install, f, &thread_ts) {
                Ok(b) => b,
                Err(e) => return StepOutcome::Retry(e),
            },
        },
        None => match bind(ctx, install, f, &thread_ts) {
            Ok(b) => b,
            Err(e) => return StepOutcome::Retry(e),
        },
    };
    st.binding = Some(binding.id.clone());
    surface.with_binding(&mut card, &binding.id);

    // The status, when it changed.
    let h = hash(&card);
    if st.hash.as_deref() != Some(h.as_str()) {
        if let Err(e) = surface.update_status(&card, &f.channel, &ts).await {
            ctx.set_state(serde_json::to_value(&st).unwrap_or(Value::Null));
            return on_failure(ctx, e);
        }
        st.hash = Some(h);
    }

    // Requests: a card for each new one, answered cards for the ones resolved elsewhere.
    for r in &v.requests {
        let row = match super::card_for(&code, &binding.id, r.id.as_str(), &f.channel) {
            Ok(row) => row,
            Err(e) => return StepOutcome::Retry(e.to_string()),
        };
        let pending = r.status == agent_proto::RequestStatus::Pending;
        if pending && row.ts.is_empty() {
            match surface.post_request(&cards::request(&row.id, r), &f.channel, &thread_ts).await {
                Ok(at) => {
                    let _ = super::set_card(&code, &row.id, Some(&at), None, None);
                }
                Err(e) => {
                    ctx.set_state(serde_json::to_value(&st).unwrap_or(Value::Null));
                    return on_failure(ctx, e);
                }
            }
        } else if !pending && row.state == "pending" {
            if !row.ts.is_empty() {
                let by = r.resolved_by.clone().unwrap_or_else(|| "someone".into());
                if let Err(e) = surface.update_request(&cards::answered(r, &by, &cards::answer_text(r)), &f.channel, &row.ts).await {
                    ctx.set_state(serde_json::to_value(&st).unwrap_or(Value::Null));
                    return on_failure(ctx, e);
                }
            }
            let _ = super::set_card(&code, &row.id, None, Some("answered"), None);
        }
    }
    ctx.set_state(serde_json::to_value(&st).unwrap_or(Value::Null));

    let mut out = v.output();
    out["channel"] = json!(f.channel);
    out["ts"] = json!(ts);
    out["thread_ts"] = json!(thread_ts);
    if f.once {
        return StepOutcome::Done(out);
    }
    match v.ended() {
        Some(Ended::Failed) => {
            out["failed"] = json!(true);
            StepOutcome::Done(out)
        }
        Some(_) => StepOutcome::Done(out),
        None => StepOutcome::Wait { after: IDLE },
    }
}

fn bind(ctx: &StepContext, install: &Install, f: &Follow, thread_ts: &str) -> Result<Binding, String> {
    let run_as = ctx.run_as.clone();
    super::bind(&ctx.code, &ctx.owner().id, &install.id, &f.channel, thread_ts, &f.machine, &f.thread, ctx.automation(), run_as.as_deref()).map_err(|e| e.to_string())
}

/// The machine and thread a step names, or the agent event's (`event.data.machine`,
/// `event.data.thread.id`: an agent event's thread is the thread's brief).
pub(crate) fn target(ctx: &StepContext, machine: Option<String>, thread: Option<String>) -> (String, String) {
    let ev = &ctx.context()["event"]["data"];
    let pick = |v: Option<String>, from: Option<&str>| v.filter(|s| !s.trim().is_empty()).or_else(|| from.map(str::to_string)).unwrap_or_default();
    (pick(machine, ev["machine"].as_str()), pick(thread, ev["thread"]["id"].as_str().or_else(|| ev["thread"].as_str())))
}
