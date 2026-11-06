//! Linear's step kinds: `linear.comment`, `linear.update_issue`, `linear.attach`,
//! `linear.create_issue` and `linear.activity` (the agent session bridge is [`super::session`]).
//! They act with the owner's connection; creates pass an id made from the step's idempotency key
//! ([`uuid_for`]), so an attempt that runs again makes nothing twice.

use super::SOURCE;
use super::client::{LinearError, already_made, uuid_for};
use super::store;
use super::trigger::{PRIORITIES, STATE_TYPES, priority_of};
use crate::agents::lookup::Pick;
use crate::Code;
use crate::agents::automations::{OwnerRef, props, StepContext, StepKind, StepOutcome};
use agent_proto::automation::StepInfo;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::sync::{Arc, Weak};

pub fn all(code: &Arc<Code>) -> Vec<Arc<dyn StepKind>> {
    let w = || Arc::downgrade(code);
    vec![Arc::new(Comment(w())), Arc::new(UpdateIssue(w())), Arc::new(Attach(w())), Arc::new(CreateIssue(w())), Arc::new(Activity(w()))]
}

/// The catalog marks a step as needing setup while the owner has no connection.
fn unset(code: &Weak<Code>, owner: &OwnerRef) -> bool {
    code.upgrade().is_some_and(|c| !connected(&c, owner))
}

/// A failed call as a step's outcome.
pub fn outcome(e: LinearError) -> StepOutcome {
    if e.transient() { StepOutcome::Retry(e.to_string()) } else { StepOutcome::Fail(e.to_string()) }
}

fn needs_setup(owner: &OwnerRef, code: &Code) -> bool {
    !matches!(store::connection(code.db.sql(), &owner.id), Ok(Some(_)))
}

/// The step's config with its templates filled in.
fn config<T: for<'de> Deserialize<'de>>(ctx: &StepContext) -> Result<T, StepOutcome> {
    let raw = if ctx.step().config.is_null() { json!({}) } else { ctx.step().config.clone() };
    let v = ctx.render_json(&raw).map_err(StepOutcome::Fail)?;
    serde_json::from_value(v).map_err(|e| StepOutcome::Fail(format!("{} config: {e}", ctx.step().kind)))
}

/// A template-filled id, else the event's issue.
fn issue_ref(given: Option<String>, ctx: &StepContext) -> Result<String, StepOutcome> {
    given
        .filter(|s| !s.trim().is_empty())
        .or_else(|| ctx.context()["event"]["data"]["issue"]["id"].as_str().map(str::to_string))
        .map(|s| s.trim().to_string())
        .ok_or_else(|| StepOutcome::Fail("no issue: set `issue` (the event has none)".into()))
}

/// A one-line template field (the builder's `template` picker).
fn text_field(title: &str, desc: &str) -> Value {
    json!({ "type": "string", "title": title, "description": desc, "format": "template" })
}

/// A multi-line template field (markdown).
fn long_field(title: &str, desc: &str) -> Value {
    json!({ "type": "string", "title": title, "description": desc, "format": "template-long" })
}

/// An issue, searched by title or identifier.
fn issue_field(desc: &str) -> Value {
    Pick::from(SOURCE, "issues").one("Issue", desc)
}

/// A team's state (its own when `team` names the sibling field), or a kind of state.
fn state_field(team: Option<&'static str>, desc: &str) -> Value {
    Pick { parent: team, extras: STATE_TYPES, extras_group: Some("Any team"), ..Pick::from(SOURCE, "states") }.one("State", desc)
}

fn assignee_field(desc: &str) -> Value {
    Pick { extras: &[("roll", "roll"), ("none", "No one")], ..Pick::from(SOURCE, "users") }.one("Assignee", desc)
}

fn delegate_field(desc: &str) -> Value {
    Pick { extras: &[("roll", "roll"), ("none", "No one")], ..Pick::default() }.one("Delegate", desc)
}

/// Labels (a team's and the workspace's when `team` names the sibling field); new ones can be made.
fn label_field(team: Option<&'static str>, create: bool) -> Pick<'static> {
    Pick { parent: team, create, ..Pick::from(SOURCE, "labels") }
}

fn priority_field() -> Value {
    Pick { extras: PRIORITIES, ..Pick::default() }.one("Priority", "How urgent it is")
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "additionalProperties": false, "properties": props, "required": required })
}

fn info(kind: &str, title: &str, description: &str, config: Value, output: &[&str]) -> StepInfo {
    StepInfo {
        kind: kind.into(),
        title: title.into(),
        description: description.into(),
        config,
        output: output.iter().map(|s| s.to_string()).collect(),
        integration: Some(SOURCE.into()),
        needs_setup: false,
    }
}

fn is_uuid(s: &str) -> bool {
    s.len() == 36 && s.chars().enumerate().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}

// --- Lookups ----------------------------------------------------------------------------------

const ISSUE: &str = "query RollIssue($id: String!) { issue(id: $id) { id identifier title url priority team { id key name } state { id name type } labels { nodes { id name } } assignee { id name } } }";

async fn issue(code: &Code, owner: &str, id: &str) -> Result<Value, LinearError> {
    let d = code.linear_graphql(owner, ISSUE, json!({ "id": id })).await?;
    if d["issue"].is_object() { Ok(d["issue"].clone()) } else { Err(LinearError::Failed(format!("Linear has no issue {id}"))) }
}

const TEAMS: &str = "query RollTeams { teams(first: 100) { nodes { id key name } } }";

/// Every team (id, key, name).
pub async fn teams(code: &Code, owner: &str) -> Result<Vec<Value>, LinearError> {
    let d = code.linear_graphql(owner, TEAMS, json!({})).await?;
    Ok(d["teams"]["nodes"].as_array().cloned().unwrap_or_default())
}

async fn team_id(code: &Code, owner: &str, team: &str) -> Result<String, LinearError> {
    if is_uuid(team) {
        return Ok(team.into());
    }
    let all = teams(code, owner).await?;
    let hit = all.iter().find(|t| t["key"].as_str().is_some_and(|k| k.eq_ignore_ascii_case(team)) || t["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(team)));
    hit.and_then(|t| t["id"].as_str()).map(str::to_string).ok_or_else(|| LinearError::Failed(format!("Linear has no team {team:?}")))
}

const STATES: &str = "query RollStates($team: ID!) { workflowStates(filter: { team: { id: { eq: $team } } }, first: 100) { nodes { id name type position } } }";

/// A team's state by name, or by type (`started`: the first of them).
async fn state_id(code: &Code, owner: &str, team: &str, want: &str) -> Result<String, LinearError> {
    if is_uuid(want) {
        return Ok(want.into());
    }
    let d = code.linear_graphql(owner, STATES, json!({ "team": team })).await?;
    let mut states = d["workflowStates"]["nodes"].as_array().cloned().unwrap_or_default();
    states.sort_by(|a, b| a["position"].as_f64().unwrap_or(0.0).total_cmp(&b["position"].as_f64().unwrap_or(0.0)));
    let by = |k: &str| states.iter().find(|s| s[k].as_str().is_some_and(|v| v.eq_ignore_ascii_case(want.trim()))).and_then(|s| s["id"].as_str()).map(str::to_string);
    by("name").or_else(|| by("type")).ok_or_else(|| LinearError::Failed(format!("the team has no state {want:?}")))
}

const LABELS: &str = "query RollLabels($name: String!) { issueLabels(filter: { name: { eqIgnoreCase: $name } }, first: 20) { nodes { id name team { id } } } }";

/// A label by name: the team's, else the workspace's.
async fn label_id(code: &Code, owner: &str, team: Option<&str>, name: &str) -> Result<String, LinearError> {
    if is_uuid(name) {
        return Ok(name.into());
    }
    let d = code.linear_graphql(owner, LABELS, json!({ "name": name.trim() })).await?;
    let ls = d["issueLabels"]["nodes"].as_array().cloned().unwrap_or_default();
    let own = ls.iter().find(|l| team.is_some() && l["team"]["id"].as_str() == team);
    let shared = ls.iter().find(|l| !l["team"].is_object());
    own.or(shared).or(ls.first()).and_then(|l| l["id"].as_str()).map(str::to_string).ok_or_else(|| LinearError::Failed(format!("Linear has no label {name:?}")))
}

const USERS: &str = "query RollUsers($q: String!) { users(filter: { or: [{ email: { eqIgnoreCase: $q } }, { displayName: { eqIgnoreCase: $q } }, { name: { eqIgnoreCase: $q } }] }, first: 1) { nodes { id name } } }";

/// A user: `roll` (roll's own user), an id, an email, a display name or a name.
async fn user_id(code: &Code, owner: &str, who: &str) -> Result<String, LinearError> {
    let who = who.trim();
    if who.eq_ignore_ascii_case("roll") {
        let c = store::connection(code.db.sql(), owner).map_err(|e| LinearError::Transient(e.to_string()))?.ok_or(LinearError::NotConnected)?;
        return Ok(c.user_id);
    }
    if is_uuid(who) {
        return Ok(who.into());
    }
    let d = code.linear_graphql(owner, USERS, json!({ "q": who })).await?;
    d["users"]["nodes"][0]["id"].as_str().map(str::to_string).ok_or_else(|| LinearError::Failed(format!("Linear has no user {who:?}")))
}

// --- linear.comment ---------------------------------------------------------------------------

const COMMENT: &str = "mutation RollComment($input: CommentCreateInput!) { commentCreate(input: $input) { success comment { id url } } }";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentConfig {
    #[serde(default)]
    issue: Option<String>,
    body: String,
    /// Reply under this comment (`{{event.data.comment.id}}`).
    #[serde(default)]
    parent: Option<String>,
}

/// Comment on an issue as roll.
pub async fn comment(code: &Code, owner: &str, issue: &str, body: &str, parent: Option<&str>, key: &str) -> Result<Value, LinearError> {
    let id = uuid_for(&format!("comment:{key}"));
    let mut input = json!({ "id": id, "issueId": issue, "body": body });
    if let Some(p) = parent.filter(|p| !p.is_empty()) {
        input["parentId"] = json!(p);
    }
    match code.linear_graphql(owner, COMMENT, json!({ "input": input })).await {
        Ok(d) => Ok(json!({ "id": d["commentCreate"]["comment"]["id"].as_str().unwrap_or(&id), "url": d["commentCreate"]["comment"]["url"] })),
        Err(e) if already_made(&e) => Ok(json!({ "id": id, "url": null })),
        Err(e) => Err(e),
    }
}

struct Comment(Weak<Code>);

#[async_trait::async_trait]
impl StepKind for Comment {
    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        unset(&self.0, owner)
    }

    fn info(&self) -> StepInfo {
        info(
            "linear.comment",
            "Comment on a Linear issue",
            "Post a markdown comment on the issue (the event's, unless `issue` says) as roll",
            schema(
                props!({
                    "issue": issue_field("Issue id or identifier (ENG-12); default: the event's issue"),
                    "body": long_field("Body", "Markdown (a template)"),
                    "parent": text_field("Parent", "Reply under this comment id"),
                }),
                &["body"],
            ),
            &["id", "url"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        serde_json::from_value::<CommentConfig>(config.clone()).map(|_| ()).map_err(|e| e.to_string())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: CommentConfig = match config(ctx) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let issue = match issue_ref(c.issue, ctx) {
            Ok(i) => i,
            Err(o) => return o,
        };
        if c.body.trim().is_empty() {
            return StepOutcome::Fail("the comment's body is empty".into());
        }
        let code = ctx.code.clone();
        match comment(&code, &ctx.owner().id.clone(), &issue, &c.body, c.parent.as_deref(), &ctx.idempotency_key()).await {
            Ok(v) => StepOutcome::Done(v),
            Err(e) => outcome(e),
        }
    }
}

// --- linear.update_issue ----------------------------------------------------------------------

const UPDATE: &str = "mutation RollIssueUpdate($id: String!, $input: IssueUpdateInput!) { issueUpdate(id: $id, input: $input) { success issue { id identifier url state { id name type } } } }";

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UpdateConfig {
    #[serde(default)]
    issue: Option<String>,
    /// A state's name (`In Progress`), type (`started`) or id.
    #[serde(default)]
    state: Option<String>,
    /// `roll`, `none`, or a user's name, email or id.
    #[serde(default)]
    assignee: Option<String>,
    /// Delegate the issue to an agent (`roll`), or `none`.
    #[serde(default)]
    delegate: Option<String>,
    /// Replace the labels.
    #[serde(default)]
    labels: Option<Vec<String>>,
    #[serde(default)]
    add_labels: Vec<String>,
    #[serde(default)]
    remove_labels: Vec<String>,
    /// 0–4, or none, urgent, high, medium, low.
    #[serde(default)]
    priority: Option<Value>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    description: Option<String>,
}

fn nobody(s: &str) -> bool {
    matches!(s.trim().to_ascii_lowercase().as_str(), "none" | "nobody" | "unassigned" | "")
}

struct UpdateIssue(Weak<Code>);

#[async_trait::async_trait]
impl StepKind for UpdateIssue {
    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        unset(&self.0, owner)
    }

    fn info(&self) -> StepInfo {
        info(
            "linear.update_issue",
            "Update a Linear issue",
            "Change the issue's state, assignee, delegate, labels, priority, title or description",
            schema(
                props!({
                    "issue": issue_field("Issue id or identifier; default: the event's issue"),
                    "state": state_field(None, "A kind of state moves it to the first state of that kind in the issue's team"),
                    "assignee": assignee_field("roll, no one, or a person"),
                    "delegate": delegate_field("roll (the agent works on it), or no one"),
                    "labels": label_field(None, true).many("Labels", "Replace the labels"),
                    "add_labels": label_field(None, true).many("Add labels", "Put these labels on it"),
                    "remove_labels": label_field(None, false).many("Remove labels", "Take these labels off it"),
                    "priority": priority_field(),
                    "title": text_field("Title", "A new title"),
                    "description": long_field("Description", "A new description (markdown)"),
                }),
                &[],
            ),
            &["id", "identifier", "url", "state"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: UpdateConfig = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        if let Some(p) = &c.priority
            && !p.as_str().is_some_and(|s| s.contains("{{"))
            && priority_of(p).is_none()
        {
            return Err(format!("priority {p} isn't 0–4 or none, urgent, high, medium, low"));
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: UpdateConfig = match config(ctx) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let id = match issue_ref(c.issue.clone(), ctx) {
            Ok(i) => i,
            Err(o) => return o,
        };
        let code = ctx.code.clone();
        let owner = ctx.owner().id.clone();
        let r = async {
            let current = issue(&code, &owner, &id).await?;
            let team = current["team"]["id"].as_str().map(str::to_string);
            let mut input = Map::new();
            if let Some(s) = c.state.as_deref().filter(|s| !s.trim().is_empty()) {
                let team = team.as_deref().ok_or_else(|| LinearError::Failed("the issue has no team".into()))?;
                input.insert("stateId".into(), json!(state_id(&code, &owner, team, s).await?));
            }
            if let Some(a) = &c.assignee {
                input.insert("assigneeId".into(), if nobody(a) { Value::Null } else { json!(user_id(&code, &owner, a).await?) });
            }
            if let Some(a) = &c.delegate {
                input.insert("delegateId".into(), if nobody(a) { Value::Null } else { json!(user_id(&code, &owner, a).await?) });
            }
            if let Some(ls) = &c.labels {
                let mut ids = Vec::new();
                for l in ls.iter().filter(|l| !l.trim().is_empty()) {
                    ids.push(label_id(&code, &owner, team.as_deref(), l).await?);
                }
                input.insert("labelIds".into(), json!(ids));
            }
            let mut add = Vec::new();
            for l in c.add_labels.iter().filter(|l| !l.trim().is_empty()) {
                add.push(label_id(&code, &owner, team.as_deref(), l).await?);
            }
            if !add.is_empty() {
                input.insert("addedLabelIds".into(), json!(add));
            }
            let mut remove = Vec::new();
            for l in c.remove_labels.iter().filter(|l| !l.trim().is_empty()) {
                remove.push(label_id(&code, &owner, team.as_deref(), l).await?);
            }
            if !remove.is_empty() {
                input.insert("removedLabelIds".into(), json!(remove));
            }
            if let Some(p) = &c.priority {
                let p = priority_of(p).ok_or_else(|| LinearError::Failed(format!("priority {p} isn't 0–4 or a name")))?;
                input.insert("priority".into(), json!(p));
            }
            if let Some(t) = c.title.as_deref().filter(|t| !t.trim().is_empty()) {
                input.insert("title".into(), json!(t));
            }
            if let Some(d) = &c.description {
                input.insert("description".into(), json!(d));
            }
            if input.is_empty() {
                return Ok(json!({ "id": current["id"], "identifier": current["identifier"], "url": current["url"], "state": current["state"]["name"], "changed": false }));
            }
            let d = code.linear_graphql(&owner, UPDATE, json!({ "id": id, "input": input })).await?;
            let i = &d["issueUpdate"]["issue"];
            Ok(json!({ "id": i["id"], "identifier": i["identifier"], "url": i["url"], "state": i["state"]["name"], "changed": true }))
        }
        .await;
        match r {
            Ok(v) => StepOutcome::Done(v),
            Err(e) => outcome(e),
        }
    }
}

// --- linear.attach ----------------------------------------------------------------------------

const ATTACH: &str = "mutation RollAttach($input: AttachmentCreateInput!) { attachmentCreate(input: $input) { success attachment { id url } } }";
const SESSION_LINK: &str = "mutation RollSessionLink($id: String!, $input: AgentSessionUpdateInput!) { agentSessionUpdate(id: $id, input: $input) { success } }";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttachConfig {
    #[serde(default)]
    issue: Option<String>,
    url: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    subtitle: Option<String>,
    #[serde(default)]
    icon_url: Option<String>,
    /// Also show the link on this agent session (default: the event's session, if any).
    #[serde(default)]
    session: Option<String>,
}

/// A link as an issue's attachment shows it.
#[derive(Clone, Copy)]
pub struct Link<'a> {
    pub url: &'a str,
    pub title: &'a str,
    pub subtitle: Option<&'a str>,
    /// A png or jpg.
    pub icon: Option<&'a str>,
}

/// Link `url` to an issue (again with the same url: the same attachment, updated) and to an
/// agent session.
pub async fn attach(code: &Code, owner: &str, issue: &str, link: &Link<'_>, session: Option<&str>) -> Result<Value, LinearError> {
    let Link { url, title, subtitle, icon } = *link;
    let mut input = json!({ "issueId": issue, "url": url, "title": title });
    if let Some(s) = subtitle.filter(|s| !s.is_empty()) {
        input["subtitle"] = json!(s);
    }
    if let Some(i) = icon.filter(|s| !s.is_empty()) {
        input["iconUrl"] = json!(i);
    }
    let d = code.linear_graphql(owner, ATTACH, json!({ "input": input })).await?;
    if let Some(s) = session.filter(|s| !s.is_empty()) {
        code.linear_graphql(owner, SESSION_LINK, json!({ "id": s, "input": { "addedExternalUrls": [{ "label": title, "url": url }] } })).await?;
    }
    Ok(json!({ "id": d["attachmentCreate"]["attachment"]["id"], "url": url }))
}

struct Attach(Weak<Code>);

#[async_trait::async_trait]
impl StepKind for Attach {
    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        unset(&self.0, owner)
    }

    fn info(&self) -> StepInfo {
        info(
            "linear.attach",
            "Link to a Linear issue",
            "Attach a link (the roll change, a PR, a preview) to the issue, and show it on the agent session",
            schema(
                props!({
                    "issue": text_field("Issue", "Issue id or identifier; default: the event's issue"),
                    "url": text_field("URL", "The link (e.g. {{steps.publish.url}})"),
                    "title": text_field("Title", "Its title (default: the URL)"),
                    "subtitle": text_field("Subtitle", "A line under it"),
                    "icon_url": text_field("Icon URL", "A png or jpg icon"),
                    "session": text_field("Session", "Also on this agent session (default: the event's)"),
                }),
                &["url"],
            ),
            &["id", "url"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        serde_json::from_value::<AttachConfig>(config.clone()).map(|_| ()).map_err(|e| e.to_string())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: AttachConfig = match config(ctx) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let issue = match issue_ref(c.issue.clone(), ctx) {
            Ok(i) => i,
            Err(o) => return o,
        };
        let url = c.url.trim().to_string();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return StepOutcome::Fail(format!("{url:?} isn't a link (an earlier step gave none?)"));
        }
        let session = c.session.clone().or_else(|| ctx.context()["event"]["data"]["session"]["id"].as_str().map(str::to_string));
        let title = c.title.clone().filter(|t| !t.trim().is_empty()).unwrap_or_else(|| url.clone());
        let code = ctx.code.clone();
        let link = Link { url: &url, title: &title, subtitle: c.subtitle.as_deref(), icon: c.icon_url.as_deref() };
        match attach(&code, &ctx.owner().id.clone(), &issue, &link, session.as_deref()).await {
            Ok(v) => StepOutcome::Done(v),
            Err(e) => outcome(e),
        }
    }
}

// --- linear.create_issue ----------------------------------------------------------------------

const CREATE: &str = "mutation RollIssueCreate($input: IssueCreateInput!) { issueCreate(input: $input) { success issue { id identifier url } } }";
const ISSUE_BY_ID: &str = "query RollIssueById($id: String!) { issue(id: $id) { id identifier url } }";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateConfig {
    /// A team's key, name or id.
    team: String,
    title: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    assignee: Option<String>,
    /// `roll`: delegate the new issue to roll (it starts an agent session).
    #[serde(default)]
    delegate: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    #[serde(default)]
    priority: Option<Value>,
    #[serde(default)]
    project: Option<String>,
    /// A parent issue (a sub-issue).
    #[serde(default)]
    parent: Option<String>,
}

struct CreateIssue(Weak<Code>);

#[async_trait::async_trait]
impl StepKind for CreateIssue {
    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        unset(&self.0, owner)
    }

    fn info(&self) -> StepInfo {
        info(
            "linear.create_issue",
            "Create a Linear issue",
            "Open an issue in a team, optionally delegated to roll",
            schema(
                props!({
                    "team": Pick::from(SOURCE, "teams").one("Team", "The team the issue goes in"),
                    "title": text_field("Title", "Title (a template)"),
                    "description": long_field("Description", "Markdown"),
                    "state": state_field(Some("team"), "Default: the team's first backlog or triage state"),
                    "assignee": assignee_field("roll, or a person"),
                    "delegate": delegate_field("roll: the agent takes it"),
                    "labels": label_field(Some("team"), true).many("Labels", "Put these labels on it"),
                    "priority": priority_field(),
                    "project": Pick::from(SOURCE, "projects").one("Project", "Add it to this project"),
                    "parent": issue_field("A sub-issue of this issue"),
                }),
                &["team", "title"],
            ),
            &["id", "identifier", "url"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        serde_json::from_value::<CreateConfig>(config.clone()).map(|_| ()).map_err(|e| e.to_string())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: CreateConfig = match config(ctx) {
            Ok(c) => c,
            Err(o) => return o,
        };
        if c.title.trim().is_empty() {
            return StepOutcome::Fail("the issue's title is empty".into());
        }
        let code = ctx.code.clone();
        let owner = ctx.owner().id.clone();
        let id = uuid_for(&format!("issue:{}", ctx.idempotency_key()));
        let r = async {
            let team = team_id(&code, &owner, c.team.trim()).await?;
            let mut input = json!({ "id": id, "teamId": team, "title": c.title.trim() });
            if let Some(d) = &c.description {
                input["description"] = json!(d);
            }
            if let Some(s) = c.state.as_deref().filter(|s| !s.trim().is_empty()) {
                input["stateId"] = json!(state_id(&code, &owner, &team, s).await?);
            }
            if let Some(a) = c.assignee.as_deref().filter(|a| !nobody(a)) {
                input["assigneeId"] = json!(user_id(&code, &owner, a).await?);
            }
            if let Some(a) = c.delegate.as_deref().filter(|a| !nobody(a)) {
                input["delegateId"] = json!(user_id(&code, &owner, a).await?);
            }
            let mut labels = Vec::new();
            for l in c.labels.iter().filter(|l| !l.trim().is_empty()) {
                labels.push(label_id(&code, &owner, Some(&team), l).await?);
            }
            if !labels.is_empty() {
                input["labelIds"] = json!(labels);
            }
            if let Some(p) = &c.priority {
                input["priority"] = json!(priority_of(p).ok_or_else(|| LinearError::Failed(format!("priority {p} isn't 0–4 or a name")))?);
            }
            if let Some(p) = c.project.as_deref().filter(|p| !p.trim().is_empty()) {
                input["projectId"] = json!(p);
            }
            if let Some(p) = c.parent.as_deref().filter(|p| !p.trim().is_empty()) {
                input["parentId"] = json!(issue(&code, &owner, p).await?["id"]);
            }
            match code.linear_graphql(&owner, CREATE, json!({ "input": input })).await {
                Ok(d) => {
                    let i = &d["issueCreate"]["issue"];
                    Ok(json!({ "id": i["id"], "identifier": i["identifier"], "url": i["url"] }))
                }
                // Made by an attempt before this one.
                Err(e) if already_made(&e) => {
                    let d = code.linear_graphql(&owner, ISSUE_BY_ID, json!({ "id": id })).await?;
                    Ok(json!({ "id": id, "identifier": d["issue"]["identifier"], "url": d["issue"]["url"] }))
                }
                Err(e) => Err(e),
            }
        }
        .await;
        match r {
            Ok(v) => StepOutcome::Done(v),
            Err(e) => outcome(e),
        }
    }
}

// --- linear.activity --------------------------------------------------------------------------

const ACTIVITY: &str = "mutation RollActivity($input: AgentActivityCreateInput!) { agentActivityCreate(input: $input) { success agentActivity { id } } }";

/// Activity content types (https://linear.app/developers/agent-interaction).
pub const ACTIVITY_TYPES: [&str; 5] = ["thought", "action", "response", "elicitation", "error"];

/// Longest body sent in one activity.
pub const ACTIVITY_BODY: usize = 8000;

/// Post an activity on an agent session. `id` makes it once (the same id again is the same
/// activity); `signal` is `select` (with `{options: [{label, value}]}`) or `auth` on an
/// elicitation, or `stop`. Returns the activity's id.
pub async fn post_activity(code: &Code, owner: &str, session: &str, mut content: Value, id: Option<&str>, ephemeral: bool) -> Result<String, LinearError> {
    let signal = content.as_object_mut().and_then(|o| o.remove("signal"));
    let meta = content.as_object_mut().and_then(|o| o.remove("signal_metadata"));
    for k in ["body", "result"] {
        if let Some(s) = content[k].as_str()
            && s.chars().count() > ACTIVITY_BODY
        {
            content[k] = json!(format!("{}…", s.chars().take(ACTIVITY_BODY).collect::<String>()));
        }
    }
    let mut input = json!({ "agentSessionId": session, "content": content });
    if let Some(id) = id {
        input["id"] = json!(id);
    }
    if ephemeral {
        input["ephemeral"] = json!(true);
    }
    if let Some(s) = signal.filter(|s| !s.is_null()) {
        input["signal"] = s;
    }
    if let Some(m) = meta.filter(|m| !m.is_null()) {
        input["signalMetadata"] = m;
    }
    match code.linear_graphql(owner, ACTIVITY, json!({ "input": input })).await {
        Ok(d) => Ok(d["agentActivityCreate"]["agentActivity"]["id"].as_str().or(id).unwrap_or_default().to_string()),
        Err(e) if already_made(&e) && id.is_some() => Ok(id.unwrap_or_default().to_string()),
        Err(e) => Err(e),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ActivityConfig {
    #[serde(default)]
    session: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    body: Option<String>,
    /// `action`: what it does (`Ran`), on what (`cargo test`), and what came of it.
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    parameter: Option<String>,
    #[serde(default)]
    result: Option<String>,
    /// Replaced by the next activity (thoughts and actions only).
    #[serde(default)]
    ephemeral: bool,
    /// An elicitation's choices (Linear shows them as buttons).
    #[serde(default)]
    options: Vec<String>,
}

struct Activity(Weak<Code>);

#[async_trait::async_trait]
impl StepKind for Activity {
    fn needs_setup(&self, owner: &OwnerRef) -> bool {
        unset(&self.0, owner)
    }

    fn info(&self) -> StepInfo {
        info(
            "linear.activity",
            "Post a Linear agent activity",
            "Show a thought, action, response, question (elicitation) or error on the agent session",
            schema(
                props!({
                    "session": text_field("Session", "Agent session id; default: the event's"),
                    "type": { "type": "string", "enum": ACTIVITY_TYPES },
                    "body": long_field("Body", "Markdown (thought, response, elicitation, error)"),
                    "action": text_field("Action", "action: what it does (Ran)"),
                    "parameter": text_field("Parameter", "action: on what (cargo test)"),
                    "result": text_field("Result", "action: what came of it"),
                    "ephemeral": { "type": "boolean" },
                    "options": { "type": "array", "items": { "type": "string" }, "description": "elicitation: choices" },
                }),
                &["type"],
            ),
            &["id"],
        )
    }

    fn check(&self, _owner: &OwnerRef, config: &Value) -> Result<(), String> {
        let c: ActivityConfig = serde_json::from_value(config.clone()).map_err(|e| e.to_string())?;
        if !ACTIVITY_TYPES.contains(&c.kind.as_str()) {
            return Err(format!("type is one of {}", ACTIVITY_TYPES.join(", ")));
        }
        if c.kind == "action" && c.action.is_none() {
            return Err("an action needs `action` (and usually `parameter`)".into());
        }
        if c.kind != "action" && c.body.is_none() {
            return Err(format!("a {} needs a body", c.kind));
        }
        if c.ephemeral && !matches!(c.kind.as_str(), "thought" | "action") {
            return Err("only thoughts and actions can be ephemeral".into());
        }
        Ok(())
    }

    async fn run(&self, ctx: &mut StepContext) -> StepOutcome {
        let c: ActivityConfig = match config(ctx) {
            Ok(c) => c,
            Err(o) => return o,
        };
        let Some(session) = c.session.clone().filter(|s| !s.is_empty()).or_else(|| ctx.context()["event"]["data"]["session"]["id"].as_str().map(str::to_string)) else {
            return StepOutcome::Fail("no agent session: set `session` (the event has none)".into());
        };
        let mut content = if c.kind == "action" {
            json!({ "type": "action", "action": c.action.unwrap_or_default(), "parameter": c.parameter.unwrap_or_default() })
        } else {
            json!({ "type": c.kind, "body": c.body.unwrap_or_default() })
        };
        if let Some(r) = c.result.filter(|_| c.kind == "action") {
            content["result"] = json!(r);
        }
        if c.kind == "elicitation" && !c.options.is_empty() {
            content["signal"] = json!("select");
            content["signal_metadata"] = json!({ "options": c.options.iter().map(|o| json!({ "label": o, "value": o })).collect::<Vec<_>>() });
        }
        let id = uuid_for(&format!("activity:{}", ctx.idempotency_key()));
        let code = ctx.code.clone();
        match post_activity(&code, &ctx.owner().id.clone(), &session, content, Some(&id), c.ephemeral).await {
            Ok(id) => StepOutcome::Done(json!({ "id": id })),
            Err(e) => outcome(e),
        }
    }
}

/// Whether the owner has Linear connected (steps' `needs_setup`).
pub fn connected(code: &Code, owner: &OwnerRef) -> bool {
    !needs_setup(owner, code)
}
