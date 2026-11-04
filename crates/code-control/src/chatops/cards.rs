//! An agent thread as chat cards: its live status (title, state, the latest run's steps, links)
//! and the requests it waits on (approvals as buttons, questions as selects), and turning a
//! click back into the request's answer. Platform-neutral: `code_chat` renders them for Slack
//! and Discord.

use crate::Code;
use agent_proto::{Entity, NodeBody, NodeStatus, Request, RequestBody, RequestStatus, Run, RunStatus, Thread, ThreadStatus};
use code_chat::card::{Choice, ChoiceStyle, Link, QuestionView, RequestCard, RequestKind, StatusCard, Tone};
use code_sql::params;
use serde_json::{Value, json};

/// Steps of the latest run shown on a status card.
const STEPS: usize = 6;
/// Records of a thread read for its card (the newest).
const READ: i64 = 400;

/// What a thread's cards are made of.
#[derive(Clone, Debug)]
pub(crate) struct ThreadView {
    pub machine_id: String,
    pub thread: Thread,
    /// Its newest run.
    pub run: Option<Run>,
    /// The newest run's tool calls and subagents, oldest first: (title, status).
    pub steps: Vec<(String, NodeStatus)>,
    /// Every request of the thread the mirror has (pending and resolved).
    pub requests: Vec<Request>,
}

/// Read a thread of `owner_id` on `machine_id` from the mirror. `None` until the daemon sent it.
pub(crate) fn view(code: &Code, owner_id: &str, machine_id: &str, thread: &str) -> crate::Result<Option<ThreadView>> {
    let db = code.db.sql();
    let Some(Entity::Thread(t)) = crate::agents::store::mirrored(db, machine_id, agent_proto::EntityKind::Thread, thread)? else { return Ok(None) };
    let rows: Vec<String> = db.query(
        "SELECT json FROM agent_entities WHERE owner_id = ? AND machine_id = ? AND thread = ? AND kind IN ('run', 'node', 'request') ORDER BY seq DESC LIMIT ?",
        params![owner_id, machine_id, thread, READ],
        |r| r.get(0),
    )?;
    let (mut runs, mut nodes, mut requests) = (Vec::new(), Vec::new(), Vec::new());
    for j in rows {
        match serde_json::from_str::<Entity>(&j) {
            Ok(Entity::Run(r)) => runs.push(r),
            Ok(Entity::Node(n)) => nodes.push(n),
            Ok(Entity::Request(r)) => requests.push(r),
            _ => {}
        }
    }
    let run = runs.into_iter().max_by_key(|r| r.ordinal);
    let mut steps: Vec<_> = nodes
        .into_iter()
        .filter(|n| run.as_ref().is_some_and(|r| r.id == n.run))
        .filter_map(|n| {
            let title = match &n.body {
                NodeBody::Tool(c) => c.title.clone(),
                NodeBody::Subagent { agent, .. } => format!("{agent} (subagent)"),
                _ => return None,
            };
            Some((n.started_at, n.ordinal, title, n.status))
        })
        .collect();
    steps.sort_by_key(|s| (s.0, s.1));
    requests.sort_by_key(|r: &Request| r.created_at);
    Ok(Some(ThreadView { machine_id: machine_id.to_string(), thread: t, run, steps: steps.into_iter().map(|s| (s.2, s.3)).collect(), requests }))
}

/// How a followed thread ended, once it has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Done,
    Failed,
    Stopped,
}

impl ThreadView {
    /// The thread's work is over: it ran, nothing runs or waits now, and its newest run ended.
    pub fn ended(&self) -> Option<Ended> {
        let t = &self.thread;
        if matches!(t.status, ThreadStatus::Starting | ThreadStatus::Running | ThreadStatus::Waiting) || t.active_run.is_some() || t.pending_requests > 0 {
            return None;
        }
        if t.status == ThreadStatus::Error {
            return Some(Ended::Failed);
        }
        match self.run.as_ref()?.status {
            RunStatus::Completed => Some(Ended::Done),
            RunStatus::Failed => Some(Ended::Failed),
            RunStatus::Interrupted | RunStatus::Cancelled | RunStatus::RolledBack => Some(Ended::Stopped),
            _ => None,
        }
    }

    /// What steps and templates see (`steps.<id>.…`).
    pub fn output(&self) -> Value {
        json!({
            "machine": self.machine_id,
            "thread": self.thread.id,
            "title": self.thread.title,
            "status": match self.ended() { Some(Ended::Done) => "done", Some(Ended::Failed) => "failed", Some(Ended::Stopped) => "stopped", None => "running" },
            "change": self.thread.change.as_ref().map(|c| json!({ "repo": c.repo, "number": c.number, "url": c.url, "status": c.status })),
            "error": self.thread.error,
        })
    }
}

/// Where the thread opens in the dashboard.
pub(crate) fn thread_url(code: &Code, owner: &str, thread: &str) -> String {
    format!("{}/dashboard/agents?owner={owner}&focus={thread}", code.config.web_url.trim_end_matches('/'))
}

/// The thread's live status card.
pub(crate) fn status(code: &Code, owner: &str, v: &ThreadView) -> StatusCard {
    let t = &v.thread;
    let (tone, word) = match (v.ended(), t.status) {
        (Some(Ended::Done), _) => (Tone::Done, "Done"),
        (Some(Ended::Failed), _) => (Tone::Failed, "Failed"),
        (Some(Ended::Stopped), _) => (Tone::Info, "Stopped"),
        (None, ThreadStatus::Waiting) => (Tone::Waiting, "Waiting on you"),
        (None, ThreadStatus::Idle) if t.run_count == 0 => (Tone::Running, "Starting"),
        (None, ThreadStatus::Starting) => (Tone::Running, "Starting"),
        (None, _) => (Tone::Running, "Running"),
    };
    let n = v.steps.len();
    let mut status = match n {
        0 => word.to_string(),
        1 => format!("{word} · 1 step"),
        n => format!("{word} · {n} steps"),
    };
    if let Some(a) = t.activity.as_ref().filter(|_| v.ended().is_none()) {
        status.push_str(&format!(" · {}", one_line(a, 80)));
    }
    let lines = v
        .steps
        .iter()
        .skip(n.saturating_sub(STEPS))
        .map(|(title, s)| {
            let mark = match s {
                NodeStatus::Completed => "✓",
                NodeStatus::Failed => "✗",
                NodeStatus::Interrupted => "–",
                NodeStatus::Running | NodeStatus::Waiting => "…",
            };
            format!("{mark} {}", one_line(title, 120))
        })
        .collect();
    let mut links = vec![Link { label: "Open in roll".into(), url: thread_url(code, owner, t.id.as_str()) }];
    if let Some(c) = &t.change {
        links.push(Link { label: format!("{}#{}", c.repo, c.number), url: c.url.clone() });
    }
    let footer = match (v.ended(), &t.error) {
        (Some(Ended::Failed), Some(e)) => Some(one_line(e, 300)),
        _ => t.change.as_ref().and_then(|c| c.detail.clone()),
    };
    StatusCard { title: one_line(&t.title, 150), tone, status, lines, links, footer }
}

fn one_line(s: &str, max: usize) -> String {
    let s: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max { s } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// A choice's value: the card and what was picked (`r:<card>:<pick>`), at most 100 bytes so it
/// fits Discord's custom ids.
fn value(card: &str, pick: &str) -> String {
    format!("r:{card}:{pick}")
}

/// What a value names: the card, and the pick (`d<i>` a decision, `s` submit, `t` free text).
pub(crate) fn parse_value(v: &str) -> Option<(&str, &str)> {
    let rest = v.strip_prefix("r:")?;
    let (card, pick) = rest.split_once(':')?;
    Some((card, pick))
}

fn decision_label(d: agent_proto::Decision) -> (&'static str, ChoiceStyle) {
    match d {
        agent_proto::Decision::Accept => ("Approve", ChoiceStyle::Primary),
        agent_proto::Decision::AcceptForSession => ("Approve for session", ChoiceStyle::Default),
        agent_proto::Decision::Decline => ("Decline", ChoiceStyle::Default),
        agent_proto::Decision::Cancel => ("Stop", ChoiceStyle::Danger),
    }
}

/// A pending request as a card with buttons (approvals) or selects (questions).
pub(crate) fn request(card_id: &str, r: &Request) -> RequestCard {
    match &r.body {
        RequestBody::Approval(a) => {
            let detail = a.command.clone().or_else(|| (!a.files.is_empty()).then(|| a.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>().join("\n"))).or_else(|| a.reason.clone());
            let choices = a
                .decisions
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let (label, style) = decision_label(*d);
                    Choice { label: label.into(), value: value(card_id, &format!("d{i}")), style }
                })
                .collect();
            RequestCard { title: one_line(&a.title, 150), detail, kind: RequestKind::Approval { choices } }
        }
        RequestBody::Questions { questions } => RequestCard {
            title: match questions.as_slice() {
                [q] => one_line(&q.question, 150),
                qs => format!("{} questions", qs.len()),
            },
            detail: None,
            kind: RequestKind::Questions {
                questions: questions
                    .iter()
                    .map(|q| QuestionView {
                        id: q.id.clone(),
                        header: q.header.clone(),
                        text: q.question.clone(),
                        options: q.options.iter().enumerate().map(|(i, o)| Choice { label: o.label.clone(), value: i.to_string(), style: ChoiceStyle::Default }).collect(),
                        multi: q.multi,
                    })
                    .collect(),
                submit: Choice { label: "Submit".into(), value: value(card_id, "s"), style: ChoiceStyle::Primary },
            },
        },
    }
}

/// A card once its request is answered (here or anywhere else).
pub(crate) fn answered(r: &Request, by: &str, answer: &str) -> RequestCard {
    let title = match &r.body {
        RequestBody::Approval(a) => one_line(&a.title, 150),
        RequestBody::Questions { questions } => questions.first().map(|q| one_line(&q.question, 150)).unwrap_or_default(),
    };
    RequestCard { title, detail: None, kind: RequestKind::Answered { by: by.into(), answer: answer.into() } }
}

/// What a resolved request's answer was, in a few words.
pub(crate) fn answer_text(r: &Request) -> String {
    match &r.response {
        Some(agent_proto::Response::Decision { decision, .. }) => decision_label(*decision).0.to_string(),
        Some(agent_proto::Response::Answers { answers }) => one_line(&answers.values().cloned().collect::<Vec<_>>().join("; "), 200),
        _ if r.status == RequestStatus::Cancelled => "cancelled".into(),
        _ => "answered".into(),
    }
}

/// The answer a click makes: `pick` from the value, `picked` the selects' values by question id
/// (option indexes, or free text). Also the answer in words, for the card.
pub(crate) fn response(r: &Request, pick: &str, picked: &[(String, Vec<String>)]) -> std::result::Result<(Value, String), String> {
    match &r.body {
        RequestBody::Approval(a) => {
            let i: usize = pick.strip_prefix('d').and_then(|i| i.parse().ok()).ok_or("not a decision")?;
            let d = *a.decisions.get(i).ok_or("not one of the request's decisions")?;
            Ok((json!({ "type": "decision", "decision": d }), decision_label(d).0.to_string()))
        }
        RequestBody::Questions { questions } => {
            if pick != "s" && pick != "t" {
                return Err("not an answer".into());
            }
            let mut answers = serde_json::Map::new();
            for q in questions {
                let got = picked.iter().find(|(id, _)| id == &q.id).map(|(_, v)| v.as_slice()).unwrap_or_default();
                let labels: Vec<String> = if q.options.is_empty() {
                    got.iter().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
                } else {
                    got.iter().filter_map(|i| i.parse::<usize>().ok()).filter_map(|i| q.options.get(i)).map(|o| o.label.clone()).collect()
                };
                if labels.is_empty() {
                    return Err(format!("answer \"{}\" first", one_line(&q.header, 60)));
                }
                answers.insert(q.id.clone(), Value::String(labels.join(", ")));
            }
            let words = answers.values().filter_map(Value::as_str).collect::<Vec<_>>().join("; ");
            Ok((json!({ "type": "answers", "answers": answers }), one_line(&words, 200)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_proto::{Approval, ApprovalAction, Decision, Question, QuestionOption};

    fn req(body: RequestBody) -> Request {
        serde_json::from_value(json!({
            "id": "req_1", "thread": "thr_1", "run": "run_1", "node": "nod_1", "status": "pending",
            "body": serde_json::to_value(&body).unwrap(), "created_at": 1
        }))
        .unwrap()
    }

    #[test]
    fn approvals_become_buttons_and_clicks_become_decisions() {
        let a: Approval = serde_json::from_value(json!({
            "action": "command", "title": "Run npm test", "command": "npm test", "input": null,
            "decisions": ["accept", "accept_for_session", "decline", "cancel"]
        }))
        .unwrap();
        assert_eq!(a.action, ApprovalAction::Command);
        let r = req(RequestBody::Approval(a));
        let card = request("chr_x", &r);
        let RequestKind::Approval { choices } = &card.kind else { panic!() };
        assert_eq!(choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Approve", "Approve for session", "Decline", "Stop"]);
        assert_eq!(card.detail.as_deref(), Some("npm test"));
        let (card_id, pick) = parse_value(&choices[2].value).unwrap();
        assert_eq!(card_id, "chr_x");
        let (resp, words) = response(&r, pick, &[]).unwrap();
        assert_eq!(resp, json!({ "type": "decision", "decision": "decline" }));
        assert_eq!(words, "Decline");
        assert!(response(&r, "d9", &[]).is_err());
        assert!(response(&r, "s", &[]).is_err());
        let _ = Decision::Accept;
    }

    #[test]
    fn questions_become_selects_and_need_every_answer() {
        let r = req(RequestBody::Questions {
            questions: vec![
                Question { id: "db".into(), header: "Database".into(), question: "Which one?".into(), options: vec![QuestionOption { label: "Postgres".into(), description: None }, QuestionOption { label: "SQLite".into(), description: None }], multi: false },
                Question { id: "why".into(), header: "Why".into(), question: "Why?".into(), options: vec![], multi: false },
            ],
        });
        let card = request("chr_y", &r);
        let RequestKind::Questions { questions, submit } = &card.kind else { panic!() };
        assert_eq!(questions[0].options[1].value, "1");
        assert_eq!(parse_value(&submit.value), Some(("chr_y", "s")));
        assert_eq!(response(&r, "s", &[("db".into(), vec!["1".into()])]).unwrap_err(), "answer \"Why\" first");
        let (resp, words) = response(&r, "s", &[("db".into(), vec!["1".into()]), ("why".into(), vec![" smaller ".into()])]).unwrap();
        assert_eq!(resp, json!({ "type": "answers", "answers": { "db": "SQLite", "why": "smaller" } }));
        assert_eq!(words, "SQLite; smaller");
        assert!(parse_value("x:chr:s").is_none());
    }
}
