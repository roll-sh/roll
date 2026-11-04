//! GitHub as automation triggers and steps (docs/code/chatops.md, "GitHub"), for the owner's
//! GitHub-connected repos.
//!
//! - **Events** come from the App's delivery inbox (`github::inbox`, durable until handled):
//!   each delivery part a connected repo handles is also an event in the repo owner's
//!   automation inbox, keyed by the delivery's id and the repo, so GitHub's redelivery records
//!   nothing twice. `issue.opened`, `issue.labeled`, `comment.mention` (`@roll …` on an issue or
//!   a pull request), `review.requested` and `check.failed` (a check run or commit status).
//! - **Authority.** A comment speaks for roll only from someone GitHub says can push
//!   (`OWNER`, `MEMBER`, `COLLABORATOR`); an issue from anyone else starts nothing unless the
//!   trigger says `anyone`. Code from outside the repo never runs on its own: an event about a
//!   fork's pull request is recorded only once a writer approved its head (`approve-run`), like
//!   checks and previews.
//! - **Steps** act on the repo the step names (one of the automation owner's connected repos;
//!   an installation spans tenants, so it's only ever reached through the owner's repo):
//!   `github.comment`, `github.label`, `github.status`, and `github.pr`, which opens a pull
//!   request from a roll change (pushing its head as a branch first).

use crate::agents::automations::{NewEvent, OwnerRef, Registry, StepContext, StepKind, StepOutcome, TriggerSource};
use crate::github::{Hub, Link, link_of};
use crate::model::RepoRow;
use crate::{Code, Result};
use agent_proto::automation::{AutomationEvent, EventInfo, StepInfo, TriggerInfo};
use code_github::Event;
use code_github::webhook::{CommentAction, IssueAction, PullRequestAction};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Weak};

pub const SOURCE: &str = "github";
/// Who GitHub says can push: their comments speak for roll.
pub const WRITERS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

pub(super) fn register(code: &Arc<Code>, r: &Registry) {
    let me = Arc::downgrade(code);
    r.register_trigger(Arc::new(GitHubTrigger(me.clone())));
    for kind in [Kind::Comment, Kind::Label, Kind::Status, Kind::Pr] {
        r.register_step(Arc::new(GitHubStep { kind, code: me.clone() }));
    }
}

fn writer(association: Option<&str>) -> bool {
    association.is_some_and(|a| WRITERS.contains(&a))
}

/// The text after `@roll` (or `@<the App's slug>`), if the comment mentions it outside code.
pub fn mention(body: &str, slug: &str) -> Option<String> {
    let names = ["@roll".to_string(), format!("@{}", slug.to_ascii_lowercase())];
    let mut in_code = false;
    for (n, line) in body.lines().enumerate() {
        let t = line.trim();
        if t.starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        for name in &names {
            let mut from = 0;
            while let Some(i) = lower[from..].find(name.as_str()) {
                let at = from + i;
                let end = at + name.len();
                let before = at == 0 || !lower.as_bytes()[at - 1].is_ascii_alphanumeric();
                let after = lower.as_bytes().get(end).is_none_or(|c| !(c.is_ascii_alphanumeric() || *c == b'-' || *c == b'_' || *c == b'['));
                if before && after {
                    let mut rest = vec![t[end..].trim_start_matches([',', ':', ' ']).trim().to_string()];
                    rest.extend(body.lines().skip(n + 1).map(str::to_string));
                    return Some(rest.join("\n").trim().to_string());
                }
                from = end;
            }
        }
    }
    None
}

/// Whether code from this pull request may run: from the repo itself, or a fork's whose head a
/// writer approved.
fn pr_trusted(code: &Code, r: &RepoRow, number: u64, head_repo: Option<&str>, base_repo: &str) -> bool {
    if let Ok(Some(id)) = crate::github::sync::change_by_pr(code, &r.id, number)
        && let Ok(Some(c)) = code.db.change(&id)
    {
        return c.trusted || c.run_approved;
    }
    head_repo.is_some_and(|h| h.eq_ignore_ascii_case(base_repo))
}

/// Record the event a delivery part is for the repo owner's automations (before the part's own
/// handling, which may fail and run again: the key makes that harmless).
pub(crate) fn record(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, receipt: &str, event: &Event) -> Result<()> {
    let Some((kind, data, summary)) = describe(code, hub, r, link, event) else { return Ok(()) };
    if !super::anyone_listens(code, &r.owner_id, SOURCE, kind) {
        return Ok(());
    }
    let e = NewEvent { source: SOURCE.into(), kind: kind.into(), id: format!("{receipt}:{}", r.id), data, summary, automation: None };
    code.automation_event(&r.owner_id, e)?;
    Ok(())
}

fn labels(ls: &[code_github::model::Label]) -> Vec<String> {
    ls.iter().map(|l| l.name.clone()).collect()
}

fn describe(code: &Code, hub: &Hub, r: &RepoRow, link: &Link, event: &Event) -> Option<(&'static str, Value, String)> {
    let bot = hub.config.bot_login();
    let is_bot = |login: Option<&str>| login.is_some_and(|l| l.eq_ignore_ascii_case(&bot));
    let base = json!({ "repo": r.full_name(), "repo_name": r.name, "github": link.full_name() });
    let with = |mut v: Value, more: Value| {
        if let (Some(a), Value::Object(b)) = (v.as_object_mut(), more) {
            a.extend(b);
        }
        v
    };
    match event {
        Event::Issues(e) if !e.issue.is_pull_request() && matches!(e.action, IssueAction::Opened | IssueAction::Labeled) => {
            let author = e.issue.user.as_ref().map(|u| u.login.clone()).unwrap_or_default();
            if is_bot(Some(&author)) && e.action == IssueAction::Opened {
                return None;
            }
            let kind = if e.action == IssueAction::Opened { "issue.opened" } else { "issue.labeled" };
            let data = with(
                base,
                json!({
                    "number": e.issue.number, "title": e.issue.title, "body": e.issue.body.clone().unwrap_or_default(), "url": e.issue.html_url,
                    "labels": labels(&e.issue.labels), "label": e.label.as_ref().map(|l| l.name.clone()), "author": author,
                    "author_association": e.issue.author_association, "trusted_author": writer(e.issue.author_association.as_deref()),
                    "sender": e.sender.as_ref().map(|u| u.login.clone()),
                }),
            );
            let summary = match &e.label {
                Some(l) if kind == "issue.labeled" => format!("{}#{} labelled {}", link.full_name(), e.issue.number, l.name),
                _ => format!("{}#{} opened: {}", link.full_name(), e.issue.number, e.issue.title),
            };
            Some((kind, data, summary))
        }
        Event::IssueComment(e) if e.action == CommentAction::Created => {
            let author = e.comment.user.as_ref().map(|u| u.login.clone()).unwrap_or_default();
            if is_bot(Some(&author)) || !writer(e.comment.author_association.as_deref()) {
                return None;
            }
            let prompt = mention(e.comment.body.as_deref().unwrap_or(""), &hub.config.slug)?;
            let is_pr = e.issue.is_pull_request();
            if is_pr && !pr_trusted(code, r, e.issue.number, None, &e.repository.full_name) {
                // Unknown pull requests count as forks until roll has them.
                tracing::debug!(repo = %r.full_name(), pr = e.issue.number, "an @roll on a fork's unapproved pull request starts nothing");
                return None;
            }
            let change = is_pr.then(|| crate::github::sync::change_by_pr(code, &r.id, e.issue.number).ok().flatten()).flatten().and_then(|id| code.db.change(&id).ok().flatten());
            let data = with(
                base,
                json!({
                    "number": e.issue.number, "title": e.issue.title, "is_pr": is_pr, "url": e.comment.html_url, "issue_url": e.issue.html_url,
                    "body": e.comment.body.clone().unwrap_or_default(), "prompt": prompt, "author": author,
                    "author_association": e.comment.author_association, "labels": labels(&e.issue.labels),
                    "change": change.as_ref().map(|c| json!({ "number": c.number, "branch": c.branch, "id": c.id })),
                }),
            );
            Some(("comment.mention", data, format!("@roll on {}#{}: {}", link.full_name(), e.issue.number, prompt.chars().take(80).collect::<String>())))
        }
        Event::PullRequest(e) if e.action == PullRequestAction::ReviewRequested => {
            let pr = &e.pull_request;
            let head_repo = pr.head.repo.as_ref().map(|r| r.full_name.as_str());
            if !pr_trusted(code, r, e.number, head_repo, &e.repository.full_name) {
                return None;
            }
            let data = with(
                base,
                json!({
                    "number": e.number, "title": pr.title, "body": pr.body.clone().unwrap_or_default(), "url": pr.html_url, "reviewer": e.requested_reviewer,
                    "author": pr.user.as_ref().map(|u| u.login.clone()), "head_ref": pr.head.ref_name, "head_sha": pr.head.sha, "base_ref": pr.base.ref_name,
                    "sender": e.sender.as_ref().map(|u| u.login.clone()),
                }),
            );
            Some(("review.requested", data, format!("review of {}#{} requested", link.full_name(), e.number)))
        }
        Event::CheckRun(e) => {
            let c = &e.check_run;
            if !matches!(c.conclusion.as_deref(), Some("failure" | "timed_out")) {
                return None;
            }
            let branch = c.check_suite.as_ref().and_then(|s| s.head_branch.clone());
            let on_default = branch.as_deref() == Some(r.default_branch.as_str());
            let prs: Vec<u64> = c.pull_requests.iter().map(|p| p.number).filter(|n| pr_trusted(code, r, *n, None, &e.repository.full_name)).collect();
            // A fork's pull request lists none here: only the default branch and the repo's own
            // pull requests start anything.
            if !on_default && prs.is_empty() {
                return None;
            }
            let data = with(
                base,
                json!({
                    "check": c.name, "conclusion": c.conclusion, "sha": c.head_sha, "branch": branch, "on_default": on_default,
                    "url": c.details_url.clone().or(c.html_url.clone()), "pull_requests": prs, "pr": prs.first(), "via": "check_run",
                    "app": c.app.as_ref().and_then(|a| a.slug.clone()),
                }),
            );
            Some(("check.failed", data, format!("{} failed on {}", c.name, branch.unwrap_or_else(|| c.head_sha.chars().take(7).collect()))))
        }
        Event::Status(e) => {
            if !matches!(e.state.as_str(), "failure" | "error") {
                return None;
            }
            let branch = e.branches.iter().find(|b| b.name == r.default_branch).or(e.branches.first()).map(|b| b.name.clone());
            let on_default = e.branches.iter().any(|b| b.name == r.default_branch);
            // A status names branches, not pull requests: only the repo's own branches.
            if branch.is_none() {
                return None;
            }
            let data = with(
                base,
                json!({
                    "check": e.context, "conclusion": e.state, "sha": e.sha, "branch": branch, "on_default": on_default, "url": e.target_url,
                    "description": e.description, "pull_requests": [], "pr": null, "via": "status",
                }),
            );
            Some(("check.failed", data, format!("{} failed on {}", e.context, branch.unwrap_or_default())))
        }
        _ => None,
    }
}

// ── the trigger ──

struct GitHubTrigger(Weak<Code>);

#[derive(Debug, Default, Deserialize)]
struct TriggerConfig {
    /// roll repo names (`app` or `owner/app`); empty: all of the owner's connected repos.
    #[serde(default)]
    repos: Vec<String>,
    /// issue.labeled: the label is one of these; issue.opened: the issue has one of them.
    #[serde(default)]
    labels: Vec<String>,
    /// check.failed: on one of these branches (empty: any).
    #[serde(default)]
    branches: Vec<String>,
    /// issue.opened / issue.labeled: from anyone (default: only people who can push).
    #[serde(default)]
    anyone: bool,
}

fn connected(code: &Code, owner_id: &str) -> Vec<RepoRow> {
    crate::github::linked_repos(code).map(|rs| rs.into_iter().filter(|r| r.owner_id == owner_id && link_of(r).is_some_and(|l| l.connected)).collect()).unwrap_or_default()
}

fn names(r: &RepoRow, name: &str) -> bool {
    name.eq_ignore_ascii_case(&r.name) || name.eq_ignore_ascii_case(&r.full_name())
}

impl TriggerSource for GitHubTrigger {
    fn info(&self) -> TriggerInfo {
        let ev = |kind: &str, title: &str, fields: &[&str]| EventInfo { kind: kind.into(), title: title.into(), fields: fields.iter().map(|f| f.to_string()).collect() };
        TriggerInfo {
            source: SOURCE.into(),
            title: "GitHub".into(),
            description: "Issues, @roll comments, review requests and failing checks on the owner's GitHub-connected repos.".into(),
            events: vec![
                ev("issue.opened", "An issue is opened", &["repo", "number", "title", "body", "labels", "author", "trusted_author"]),
                ev("issue.labeled", "An issue is labelled", &["repo", "number", "title", "label", "labels"]),
                ev("comment.mention", "@roll in a comment", &["repo", "number", "is_pr", "prompt", "body", "author", "change.number"]),
                ev("review.requested", "A review is requested", &["repo", "number", "title", "reviewer", "head_ref"]),
                ev("check.failed", "A check fails", &["repo", "check", "branch", "on_default", "sha", "pr", "url"]),
            ],
            config: json!({
                "type": "object",
                "properties": {
                    "repos": { "type": "array", "items": { "type": "string" }, "description": "Repos (names); unset: every connected repo." },
                    "labels": { "type": "array", "items": { "type": "string" }, "description": "issue.labeled: one of these labels; issue.opened: having one." },
                    "branches": { "type": "array", "items": { "type": "string" }, "description": "check.failed: on these branches." },
                    "anyone": { "type": "boolean", "description": "Issues from anyone, not just people who can push." }
                }
            }),
            needs_setup: false,
        }
    }

    fn check(&self, owner: &OwnerRef, events: &[String], config: &Value) -> std::result::Result<(), String> {
        let c: TriggerConfig = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("github trigger config: {e}"))?;
        let known = ["issue.opened", "issue.labeled", "comment.mention", "review.requested", "check.failed"];
        if let Some(k) = events.iter().find(|k| !known.contains(&k.as_str())) {
            return Err(format!("github has no event {k}"));
        }
        let Some(code) = self.0.upgrade() else { return Err("shutting down".into()) };
        if code.github_hub().is_none() {
            return Err("the GitHub App isn't configured on this server".into());
        }
        let repos = connected(&code, &owner.id);
        if repos.is_empty() {
            return Err("no repo of this owner is connected to GitHub".into());
        }
        if let Some(missing) = c.repos.iter().find(|n| !repos.iter().any(|r| names(r, n))) {
            return Err(format!("{missing} isn't one of the owner's GitHub-connected repos"));
        }
        Ok(())
    }

    fn matches(&self, config: &Value, event: &AutomationEvent) -> bool {
        let c: TriggerConfig = serde_json::from_value(config.clone()).unwrap_or_default();
        let d = &event.data;
        let repo = d["repo"].as_str().unwrap_or_default();
        let name = d["repo_name"].as_str().unwrap_or_default();
        if !c.repos.is_empty() && !c.repos.iter().any(|n| n.eq_ignore_ascii_case(repo) || n.eq_ignore_ascii_case(name)) {
            return false;
        }
        let has = |l: &str| c.labels.iter().any(|x| x.eq_ignore_ascii_case(l));
        match event.kind.as_str() {
            "issue.opened" | "issue.labeled" if !c.anyone && !d["trusted_author"].as_bool().unwrap_or(false) && event.kind == "issue.opened" => false,
            "issue.labeled" => c.labels.is_empty() || d["label"].as_str().is_some_and(has),
            "issue.opened" => c.labels.is_empty() || d["labels"].as_array().is_some_and(|ls| ls.iter().filter_map(Value::as_str).any(has)),
            "check.failed" => c.branches.is_empty() || d["branch"].as_str().is_some_and(|b| c.branches.iter().any(|x| x == b)),
            _ => true,
        }
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        self.0.upgrade().is_none_or(|code| code.github_hub().is_none() || connected(&code, &owner.id).is_empty())
    }
}

// ── steps ──

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Comment,
    Label,
    Status,
    Pr,
}

struct GitHubStep {
    kind: Kind,
    code: Weak<Code>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default)]
    repo: Option<String>,
    #[serde(default)]
    number: Option<Value>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    add: Vec<String>,
    #[serde(default)]
    remove: Vec<String>,
    #[serde(default)]
    sha: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
    #[serde(default)]
    change: Option<Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    draft: Option<Value>,
}

fn num(v: &Option<Value>) -> Option<u64> {
    match v.as_ref()? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().trim_start_matches('#').parse().ok(),
        _ => None,
    }
}

impl GitHubStep {
    fn kind_name(&self) -> &'static str {
        match self.kind {
            Kind::Comment => "github.comment",
            Kind::Label => "github.label",
            Kind::Status => "github.status",
            Kind::Pr => "github.pr",
        }
    }

    fn schema(&self) -> Value {
        let s = |d: &str| json!({ "type": "string", "description": d });
        let repo = s("A GitHub-connected repo of the owner; unset: the event's.");
        let number = json!({ "type": ["integer", "string"], "description": "Issue or pull request number; unset: the event's." });
        let list = |d: &str| json!({ "type": "array", "items": { "type": "string" }, "description": d });
        let (props, required) = match self.kind {
            Kind::Comment => (json!({ "repo": repo, "number": number, "body": s("Markdown (a template).") }), json!(["body"])),
            Kind::Label => (json!({ "repo": repo, "number": number, "add": list("Labels to add."), "remove": list("Labels to take off.") }), json!([])),
            Kind::Status => (
                json!({
                    "repo": repo, "sha": s("The commit; unset: the event's."), "state": { "type": "string", "enum": ["pending", "success", "failure", "error"] },
                    "context": s("The status's name (default roll/automation)."), "description": s("One line."), "target_url": s("A link.")
                }),
                json!(["state"]),
            ),
            Kind::Pr => (
                json!({
                    "repo": repo, "change": json!({ "type": ["integer", "string"], "description": "The roll change (steps.<publish>.change)." }),
                    "title": s("Unset: the change's title."), "body": s("Unset: the change's description."), "draft": { "type": ["boolean", "string"] }
                }),
                json!(["change"]),
            ),
        };
        json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
    }
}

#[async_trait::async_trait]
impl StepKind for GitHubStep {
    fn info(&self) -> StepInfo {
        let (title, description, output): (&str, &str, &[&str]) = match self.kind {
            Kind::Comment => ("Comment on GitHub", "Comment on an issue or pull request.", &["id", "url"]),
            Kind::Label => ("Label on GitHub", "Add or remove labels on an issue or pull request.", &["labels"]),
            Kind::Status => ("Set a commit status", "Set a commit status on GitHub.", &["state", "context"]),
            Kind::Pr => ("Open a pull request", "Open a GitHub pull request from a roll change (its head pushed as a branch).", &["number", "url", "branch"]),
        };
        StepInfo {
            kind: self.kind_name().into(),
            title: title.into(),
            description: description.into(),
            config: self.schema(),
            output: output.iter().map(|s| s.to_string()).collect(),
            integration: Some(SOURCE.into()),
            needs_setup: false,
        }
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> std::result::Result<(), String> {
        let kind = self.kind_name();
        let c: Config = serde_json::from_value(if config.is_null() { json!({}) } else { config.clone() }).map_err(|e| format!("{kind} config: {e}"))?;
        match self.kind {
            Kind::Comment if c.body.as_deref().is_none_or(|b| b.trim().is_empty()) => Err("github.comment needs a body".into()),
            Kind::Label if c.add.is_empty() && c.remove.is_empty() => Err("github.label needs labels to add or remove".into()),
            Kind::Status if c.state.is_none() => Err("github.status needs a state".into()),
            Kind::Pr if c.change.is_none() => Err("github.pr needs the change".into()),
            _ => Ok(()),
        }
    }

    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        self.code.upgrade().is_none_or(|code| code.github_hub().is_none() || connected(&code, &owner.id).is_empty())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let code = ctx.code.clone();
        let kind = self.kind_name();
        let Some(hub) = code.github_hub().cloned() else { return StepOutcome::Fail("the GitHub App isn't configured on this server".into()) };
        let rendered = match ctx.render_json(&ctx.step().config.clone()) {
            Ok(v) => v,
            Err(e) => return StepOutcome::Fail(e),
        };
        let c: Config = match serde_json::from_value(if rendered.is_null() { json!({}) } else { rendered }) {
            Ok(c) => c,
            Err(e) => return StepOutcome::Fail(format!("{kind} config: {e}")),
        };
        let ev = ctx.context()["event"].clone();
        let from_github = ev["source"] == SOURCE;
        let repo_name = c.repo.clone().filter(|s| !s.trim().is_empty()).or_else(|| if from_github { ev["data"]["repo"].as_str().map(str::to_string) } else { None });
        let Some(repo_name) = repo_name else { return StepOutcome::Fail(format!("{kind} needs a repo")) };
        // Only the automation owner's own connected repos: that's what reaches the installation.
        let Some(r) = connected(&code, &ctx.owner().id).into_iter().find(|r| names(r, &repo_name)) else {
            return StepOutcome::Fail(format!("{repo_name} isn't one of this owner's GitHub-connected repos"));
        };
        let Some(link) = link_of(&r) else { return StepOutcome::Fail("not connected".into()) };
        let gh = |e: code_github::Error| {
            let transient = e.status().is_none_or(|s| s >= 500 || s == 429 || s == 408);
            if transient { StepOutcome::Retry(e.to_string()) } else { StepOutcome::Fail(e.to_string()) }
        };
        let number = num(&c.number).or_else(|| if from_github { ev["data"]["pr"].as_u64().or(ev["data"]["number"].as_u64()) } else { None });
        let (inst, owner, name) = (link.installation_id, link.owner.as_str(), link.repo.as_str());
        match self.kind {
            Kind::Comment => {
                let Some(n) = number else { return StepOutcome::Fail("github.comment needs a number".into()) };
                let once = format!("gh-comment:{}", ctx.idempotency_key());
                if let Some(done) = super::mark(&code, &once).and_then(|m| serde_json::from_str::<Value>(&m).ok()) {
                    return StepOutcome::Done(done);
                }
                match hub.client.create_comment(inst, owner, name, n, c.body.as_deref().unwrap_or_default()).await {
                    Ok(cm) => {
                        let out = json!({ "id": cm.id, "url": cm.html_url, "number": n });
                        super::set_mark(&code, &once, &out.to_string());
                        StepOutcome::Done(out)
                    }
                    Err(e) => gh(e),
                }
            }
            Kind::Label => {
                let Some(n) = number else { return StepOutcome::Fail("github.label needs a number".into()) };
                let mut labels = Vec::new();
                if !c.add.is_empty() {
                    match hub.client.add_labels(inst, owner, name, n, &c.add).await {
                        Ok(ls) => labels = ls.into_iter().map(|l| l.name).collect(),
                        Err(e) => return gh(e),
                    }
                }
                for l in &c.remove {
                    if let Err(e) = hub.client.remove_label(inst, owner, name, n, l).await {
                        return gh(e);
                    }
                    labels.retain(|x| x != l);
                }
                StepOutcome::Done(json!({ "labels": labels, "number": n }))
            }
            Kind::Status => {
                let state = c.state.clone().unwrap_or_default();
                if !["pending", "success", "failure", "error"].contains(&state.as_str()) {
                    return StepOutcome::Fail(format!("{state} isn't a commit status (pending, success, failure, error)"));
                }
                let sha = c.sha.clone().filter(|s| !s.is_empty()).or_else(|| if from_github { ev["data"]["sha"].as_str().or(ev["data"]["head_sha"].as_str()).map(str::to_string) } else { None });
                let Some(sha) = sha else { return StepOutcome::Fail("github.status needs a sha".into()) };
                let context = c.context.clone().filter(|s| !s.is_empty()).unwrap_or_else(|| "roll/automation".into());
                let desc: Option<String> = c.description.as_deref().map(|d| d.chars().take(140).collect());
                match hub.client.create_status(inst, owner, name, &sha, &state, &context, desc.as_deref(), c.target_url.as_deref().filter(|u| !u.is_empty())).await {
                    Ok(_) => StepOutcome::Done(json!({ "state": state, "context": context, "sha": sha })),
                    Err(e) => gh(e),
                }
            }
            Kind::Pr => open_pr(&code, &hub, &r, &link, &c).await,
        }
    }
}

async fn open_pr(code: &Arc<Code>, hub: &Hub, r: &RepoRow, link: &Link, c: &Config) -> StepOutcome {
    let Some(n) = num(&c.change) else { return StepOutcome::Fail("github.pr needs the change's number".into()) };
    let ch = match code.db.change_by_number(&r.id, n) {
        Ok(Some(ch)) => ch,
        Ok(None) => return StepOutcome::Fail(format!("{} has no change {n}", r.full_name())),
        Err(e) => return StepOutcome::Retry(e.to_string()),
    };
    if let (Some(pr), Some(url)) = (ch.github_pr, ch.github_url.clone()) {
        return StepOutcome::Done(json!({ "number": pr, "url": url, "branch": ch.branch }));
    }
    let head = match code.store.resolve(&r.id, &ch.head) {
        Ok(Some(cs)) => cs,
        Ok(None) => return StepOutcome::Fail(format!("change {n}'s head isn't in the repo")),
        Err(e) => return StepOutcome::Retry(e.to_string()),
    };
    let commit = match code.store.derive_git(&r.id, &head) {
        Ok((commit, _)) => commit.hex(),
        Err(e) => return StepOutcome::Retry(e.to_string()),
    };
    let branch = ch.branch.clone().filter(|b| !b.is_empty() && *b != r.default_branch).unwrap_or_else(|| format!("roll/change-{n}"));
    if let Err((transient, why)) = crate::github::mirror::push_branch(code, &r.id, &branch, &commit).await {
        return if transient { StepOutcome::Retry(why) } else { StepOutcome::Fail(why) };
    }
    let (inst, owner, name) = (link.installation_id, link.owner.as_str(), link.repo.as_str());
    let gh = |e: code_github::Error| if e.status().is_none_or(|s| s >= 500 || s == 429) { StepOutcome::Retry(e.to_string()) } else { StepOutcome::Fail(e.to_string()) };
    // A pull request from the branch already (an attempt that ran before): that one.
    let pr = match hub.client.open_pull_request_from(inst, owner, name, &branch).await {
        Ok(Some(pr)) => pr,
        Ok(None) => {
            let title = c.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| ch.title.clone());
            let body = c.body.clone().unwrap_or_else(|| ch.body.clone());
            let draft = match &c.draft {
                Some(Value::Bool(b)) => *b,
                Some(Value::String(s)) => s == "true",
                _ => ch.draft,
            };
            match hub.client.create_pull_request(inst, owner, name, &title, &branch, &r.default_branch, &body, draft).await {
                Ok(pr) => pr,
                Err(e) => return gh(e),
            }
        }
        Err(e) => return gh(e),
    };
    // The change is the pull request's from now on (its webhook finds it, not a new change).
    let url = pr.html_url.clone().unwrap_or_default();
    if let Err(e) = code.db.sql().execute("UPDATE changes SET github_pr = ?, github_url = ? WHERE id = ?", code_sql::params![pr.number as i64, url, ch.id]) {
        return StepOutcome::Retry(e.to_string());
    }
    StepOutcome::Done(json!({ "number": pr.number, "url": url, "branch": branch, "change": n }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mentions_outside_code_only() {
        assert_eq!(mention("@roll fix the login test", "roll-app").as_deref(), Some("fix the login test"));
        assert_eq!(mention("Hey @Roll-App: please\nadd tests", "roll-app").as_deref(), Some("please\nadd tests"));
        assert_eq!(mention("ask @rollbar about it", "roll-app"), None);
        assert_eq!(mention("```\n@roll fix\n```", "roll-app"), None);
        assert_eq!(mention("mail me@roll.dev", "roll-app"), None);
        assert_eq!(mention("@roll[bot] hi", "roll-app"), None);
    }

    #[test]
    fn only_writers_speak_for_roll() {
        for a in WRITERS {
            assert!(writer(Some(a)));
        }
        for a in ["CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "FIRST_TIMER", "MANNEQUIN", "NONE"] {
            assert!(!writer(Some(a)), "{a}");
        }
        assert!(!writer(None));
    }
}
