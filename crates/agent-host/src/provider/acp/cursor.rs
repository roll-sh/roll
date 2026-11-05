//! Cursor (`cursor-agent acp`): what it needs beyond the protocol. Pure, like
//! `antigravity.rs`; the hooks live in `acp.rs`, `session.rs`, `translate.rs` and `policy.rs`
//! behind [`Flavor::Cursor`](super::wire::Flavor).
//!
//! T3 Code (MIT) runs Cursor through its in-process SDK with an API key, which roll never holds,
//! so roll keeps Cursor's own ACP server and ports what doesn't touch a credential, from
//! `packages/provider-cursor/src/server/`:
//! - `CursorSdkCatalog.ts`, `sdkModel.ts` and `status.ts` (`buildCursorCapabilitiesFromSdkModel`,
//!   `cursorSdkModelSelection`, `cursorContextWindowTokens`): a model's parameters as options in
//!   T3's order (effort or reasoning, context, fast, thinking, the rest), true/false parameters as
//!   switches, `context` as the context window and `fast` as the speed, `200k`/`1m` windows, and
//!   `auto` as Cursor's `default`.
//! - `skills.ts`: `$name` written as Cursor's `/name` for the SKILL.md skills found under
//!   `{cwd,~}/{.cursor,.agents,.codex,.claude}/skills` (and Cursor's synced `~/.cursor/skills-cursor`),
//!   scanned within T3's bounds, reading only names and a skill's `surfaces`.
//! - `adapter.ts`: `updateTodos` as todos, `createPlan` as a plan, `task` as a subagent.
//!
//! The ACP shapes come from cursor-agent 2026.11.05-86047df itself: its `initialize` answer
//! (no `agentInfo`; the `cursor_login` method says it's Cursor), and its ACP server's source
//! (`src/acp/*`): the parameterized model picker a client opts into with
//! `clientCapabilities._meta.parameterizedModelPicker`, parameters as `model_config` or
//! `thought_level` selects (switches as `"true"`/`"false"`), `rawInput._toolName` on its own
//! tools, the `cursor/update_todos`, `cursor/task`, `cursor/ask_question` and `cursor/create_plan`
//! requests, `subagent_spawned`/`subagent_state_update`, and `cursor-agent status --format json`.
//!
//! Left out (docs/code/agents.md): T3's API-key catalog, its account and usage readers
//! (`CursorAccountReader.ts`, `CursorUsageAccounts.ts`, `accountUsage.ts`, `usageLimits.ts`),
//! the keychain and `~/.cursor/auth.json` (`CursorKeychain.ts`, `credentialStore.ts`, `auth.ts`):
//! each reads a Cursor credential or calls Cursor's API with one.

use agent_proto::{AuthStatus, ModelInfo, ModelOption, OptionRole, Question, QuestionOption, Todo, TodoStatus};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

/// The `[[acp]]` name roll knows it by.
pub const NAME: &str = "cursor";
pub const BINARY: &str = "cursor-agent";
/// The only sign-in method it offers; nothing else in `initialize` names it.
pub const AUTH_METHOD: &str = "cursor_login";
/// What its status says when it isn't signed in.
pub const SIGN_IN: &str = "Not logged in: run `cursor-agent login` in a terminal";

/// Whether the agent configured as `name` running `binary` is Cursor, before it answers.
pub fn expected(name: &str, binary: &Path) -> bool {
    name == NAME || binary.file_name().and_then(|n| n.to_str()) == Some(BINARY)
}

/// The `clientCapabilities._meta` that asks for a model's parameters as options of their own
/// (its "parameterized" picker) instead of every combination as a model.
pub fn client_meta() -> Value {
    json!({ "parameterizedModelPicker": true })
}

// ---------------------------------------------------------------------------------------------
// Models
// ---------------------------------------------------------------------------------------------

/// T3's `cursorSdkParameterPriority`.
fn priority(id: &str) -> u8 {
    match id {
        "effort" | "reasoning" => 0,
        "context" => 1,
        "fast" => 2,
        "thinking" => 3,
        _ => 4,
    }
}

/// A model's options as T3 shows Cursor's (`buildCursorCapabilitiesFromSdkModel`): in its order,
/// a `true`/`false` select as a switch, `context` the context window, `fast` the speed.
pub fn options(options: Vec<ModelOption>) -> Vec<ModelOption> {
    let mut out: Vec<ModelOption> = options.into_iter().map(option).collect();
    // The reasoning option rides in the thread's effort; it stays first.
    out.sort_by_key(|o| if o.role() == Some(OptionRole::Reasoning) { 0 } else { 1 + priority(o.id()) });
    out
}

fn option(o: ModelOption) -> ModelOption {
    let role = |id: &str, role: Option<OptionRole>| match id {
        "context" => Some(OptionRole::ContextWindow),
        "fast" => Some(OptionRole::Speed),
        _ => role,
    };
    match o {
        ModelOption::Select { id, label, description, choices, default, role: r } => {
            let ids: BTreeSet<String> = choices.iter().map(|c| c.id.to_lowercase()).collect();
            if r != Some(OptionRole::Reasoning) && choices.len() == 2 && ids.contains("true") && ids.contains("false") {
                let default = default.is_some_and(|d| d.eq_ignore_ascii_case("true"));
                let role = role(&id, None);
                return ModelOption::Boolean { id, label, description, default, role };
            }
            let role = if r == Some(OptionRole::Reasoning) { r } else { role(&id, r) };
            ModelOption::Select { id, label, description, choices, default, role }
        }
        ModelOption::Boolean { id, label, description, default, role: r } => {
            let role = role(&id, r);
            ModelOption::Boolean { id, label, description, default, role }
        }
    }
}

/// The catalog a session reported, with its options as Cursor's.
pub fn catalog(models: Vec<ModelInfo>) -> Vec<ModelInfo> {
    models.into_iter().map(|m| ModelInfo { options: options(m.options.clone()), ..m }).collect()
}

/// The model to write: T3 picks `auto` where Cursor's own id is `default`.
pub fn model<'a>(wanted: &'a str, offered: &[&str]) -> &'a str {
    if wanted == "auto" && !offered.contains(&"auto") && offered.contains(&"default") { "default" } else { wanted }
}

/// `200k` or `1m` as tokens (T3's `cursorContextWindowTokens`).
pub fn context_tokens(value: &str) -> Option<u64> {
    let v = value.trim().to_lowercase();
    let (digits, unit) = v.split_at(v.len().checked_sub(1)?);
    let scale = match unit {
        "k" => 1_000,
        "m" => 1_000_000,
        _ => return None,
    };
    digits.parse::<u64>().ok().filter(|n| *n > 0)?.checked_mul(scale)
}

// ---------------------------------------------------------------------------------------------
// Its own tools and requests
// ---------------------------------------------------------------------------------------------

/// `rawInput._toolName`: its name for a tool of its own (`task`, `updateTodos`, …).
pub fn tool_name(update: &Value) -> Option<&str> {
    update["rawInput"]["_toolName"].as_str()
}

/// Tools shown some other way: `updateTodos` as the todos, `createPlan` as the plan.
pub fn hidden(name: &str) -> bool {
    matches!(name, "updateTodos" | "createPlan")
}

/// What Cursor asks of roll beyond ACP.
#[derive(Clone, Debug, PartialEq)]
pub enum Extension {
    /// `cursor/update_todos`: the list, or changes to it by id (`merge`).
    Todos { todos: Vec<(String, Todo)>, merge: bool },
    /// `cursor/task`: what a subagent tool ran.
    Task { tool: String, model: Option<String> },
    /// `cursor/ask_question`: questions with its own option ids.
    Ask(Ask),
    /// `cursor/create_plan`: a plan, accepted as it's shown (the build is the next turn).
    Plan { tool: Option<String>, text: String },
}

/// One of its extension requests (`cursor/…`, or `_cursor/…` as ACP names extensions).
pub fn extension(method: &str, params: &Value) -> Option<Extension> {
    let text = |v: &Value| super::wire::text(v);
    Some(match method.trim_start_matches('_') {
        "cursor/update_todos" => {
            let todos = super::wire::array(&params["todos"])
                .iter()
                .filter_map(|t| {
                    let status = match t["status"].as_str() {
                        Some("in_progress") => TodoStatus::InProgress,
                        Some("completed") => TodoStatus::Completed,
                        // T3 drops cancelled steps.
                        Some("cancelled") => return None,
                        _ => TodoStatus::Pending,
                    };
                    Some((t["id"].as_str()?.to_string(), Todo { text: text(&t["content"])?, status }))
                })
                .collect();
            Extension::Todos { todos, merge: params["merge"].as_bool().unwrap_or(false) }
        }
        "cursor/task" => Extension::Task { tool: text(&params["toolCallId"])?, model: text(&params["model"]) },
        "cursor/ask_question" => Extension::Ask(ask(params)?),
        "cursor/create_plan" => {
            let plan = text(&params["plan"]).or_else(|| text(&params["overview"]))?;
            let text = match text(&params["name"]) {
                Some(name) => format!("# {name}\n\n{plan}"),
                None => plan,
            };
            Extension::Plan { tool: super::wire::text(&params["toolCallId"]), text }
        }
        _ => return None,
    })
}

/// The answer to an extension that needs none from the user.
pub fn acknowledged(ext: &Extension) -> Value {
    match ext {
        Extension::Plan { .. } => json!({ "outcome": { "outcome": "accepted" } }),
        _ => json!({}),
    }
}

/// Its todo list, kept by id so `merge` updates can change it.
#[derive(Debug, Default)]
pub struct Todos(Vec<(String, Todo)>);

/// What a session keeps for Cursor: its todos, and the tools shown some other way.
#[derive(Debug, Default)]
pub struct State {
    pub todos: Todos,
    pub hidden: HashSet<String>,
}

impl Todos {
    pub fn apply(&mut self, todos: Vec<(String, Todo)>, merge: bool) -> Vec<Todo> {
        if !merge {
            self.0.clear();
        }
        for (id, todo) in todos {
            match self.0.iter_mut().find(|(i, _)| *i == id) {
                Some((_, t)) => *t = todo,
                None => self.0.push((id, todo)),
            }
        }
        self.0.iter().map(|(_, t)| t.clone()).collect()
    }
}

/// `cursor/ask_question`: the questions, and each one's option ids by label.
#[derive(Clone, Debug, PartialEq)]
pub struct Ask {
    pub tool: Option<String>,
    pub questions: Vec<Question>,
    pub options: Vec<(String, Vec<(String, String)>)>,
}

fn ask(params: &Value) -> Option<Ask> {
    let title = super::wire::text(&params["title"]);
    let mut questions = Vec::new();
    let mut options = Vec::new();
    for (i, q) in super::wire::array(&params["questions"]).iter().enumerate() {
        let id = super::wire::text(&q["id"])?;
        let mut choices: Vec<(String, String)> = Vec::new();
        for o in super::wire::array(&q["options"]) {
            let option = super::wire::text(&o["id"])?;
            let mut label = super::wire::text(&o["label"]).unwrap_or_else(|| option.clone());
            if choices.iter().any(|(_, l)| *l == label) {
                label = format!("{label} ({option})");
            }
            choices.push((option, label));
        }
        questions.push(Question {
            id: id.clone(),
            header: title.clone().unwrap_or_else(|| format!("Question {}", i + 1)),
            question: super::wire::text(&q["prompt"]).unwrap_or_default(),
            options: choices.iter().map(|(_, label)| QuestionOption { label: label.clone(), description: None }).collect(),
            multi: q["allowMultiple"].as_bool().unwrap_or(false),
        });
        options.push((id, choices));
    }
    (!questions.is_empty()).then(|| Ask { tool: super::wire::text(&params["toolCallId"]), questions, options })
}

/// The answer to `cursor/ask_question`: each question's picked option ids. A label that isn't
/// one of its options is refused; a question left unanswered is left out.
pub fn answered(options: &[(String, Vec<(String, String)>)], answers: &BTreeMap<String, String>) -> Result<Value, String> {
    let mut out = Vec::new();
    for (question, choices) in options {
        let Some(answer) = answers.get(question) else { continue };
        let picked: Vec<&str> = answer.split(", ").filter(|a| !a.is_empty()).collect();
        let mut ids = Vec::new();
        for label in picked {
            let id = choices.iter().find(|(id, l)| l == label || id == label).ok_or_else(|| format!("{label:?} isn't one of the answers offered"))?;
            ids.push(id.0.clone());
        }
        out.push(json!({ "questionId": question, "selectedOptionIds": ids }));
    }
    Ok(json!({ "outcome": { "outcome": "answered", "answers": out } }))
}

/// Declined: skipped (the agent goes on without answers). Cancelled: cancelled.
pub fn unanswered(cancelled: bool) -> Value {
    match cancelled {
        true => json!({ "outcome": { "outcome": "cancelled" } }),
        false => json!({ "outcome": { "outcome": "skipped", "reason": "The user skipped the questions" } }),
    }
}

/// What a tool of its own adds beyond ACP's fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolExtra {
    /// The subagent's model (`cursor/task`, `subagent_spawned`).
    pub model: Option<String>,
    /// How the subagent ended (`subagent_state_update`); a background one outlives its tool.
    pub ended: Option<agent_proto::NodeStatus>,
}

/// `subagent_spawned` or `subagent_state_update`, for the `task` tool that started it.
pub fn subagent_update(update: &Value) -> Option<(String, ToolExtra)> {
    let meta = &update["_meta"]["cursor"];
    let tool = super::wire::text(&meta["toolCallId"])?;
    let ended = match update["state"].as_str() {
        Some("completed") => Some(agent_proto::NodeStatus::Completed),
        Some("failed") => Some(agent_proto::NodeStatus::Failed),
        Some("cancelled" | "disconnected") => Some(agent_proto::NodeStatus::Interrupted),
        _ => None,
    };
    Some((tool, ToolExtra { model: super::wire::text(&meta["model"]), ended }))
}

// ---------------------------------------------------------------------------------------------
// Status
// ---------------------------------------------------------------------------------------------

/// The arguments of its own status check (it reads its own login and prints the outcome).
pub const STATUS_ARGS: [&str; 3] = ["status", "--format", "json"];

/// `cursor-agent status --format json`: `status` is `authenticated`, `partially-authenticated`,
/// `unauthenticated` or `error`. Only the status and the account's email are read.
pub fn auth(stdout: &str) -> (AuthStatus, Option<String>) {
    let Ok(v) = serde_json::from_str::<Value>(stdout.trim()) else { return (AuthStatus::Unknown, None) };
    match v["status"].as_str() {
        Some("authenticated") => {
            let detail = super::wire::text(&v["userInfo"]["email"]).map(|e| format!("Logged in as {e}")).unwrap_or_else(|| "Logged in".into());
            (AuthStatus::Ready, Some(detail))
        }
        Some("unauthenticated" | "partially-authenticated") => (AuthStatus::Missing, Some(SIGN_IN.into())),
        _ => (AuthStatus::Unknown, None),
    }
}

// ---------------------------------------------------------------------------------------------
// Skills
// ---------------------------------------------------------------------------------------------

const SKILL_DEPTH: usize = 10;
const SKILL_BYTES: u64 = 1_000_000;
const SCAN_ENTRIES: usize = 10_000;
const SCAN_STATS: usize = 100_000;
const SCAN_BYTES: u64 = 8_000_000;

struct Budget {
    entries: usize,
    stats: usize,
    bytes: u64,
}

/// The names of the skills Cursor can run in `cwd` (T3's `inspectCursorSkills`): every directory
/// holding a `SKILL.md` under the skill roots, project first. Symlinked roots are scanned once; a
/// link out of a root shows its own `SKILL.md` but is never walked; `node_modules` is skipped.
pub fn skill_names(cwd: &Path, home: Option<&Path>) -> BTreeSet<String> {
    let below = |base: &Path| [".cursor", ".agents", ".codex", ".claude"].map(|d| base.join(d).join("skills"));
    let mut roots: Vec<PathBuf> = below(cwd).into();
    if let Some(home) = home {
        roots.extend(below(home));
        roots.push(home.join(".cursor").join("skills-cursor"));
    }
    let mut budget = Budget { entries: SCAN_ENTRIES, stats: SCAN_STATS, bytes: SCAN_BYTES };
    let mut scanned = HashSet::new();
    let mut names = BTreeSet::new();
    for root in roots {
        let Ok(real) = std::fs::canonicalize(&root) else { continue };
        if !scanned.insert(real.clone()) {
            continue;
        }
        let mut visited = HashSet::new();
        visit(&real, &real, 0, &mut budget, &mut visited, &mut names);
    }
    names
}

fn visit(root: &Path, dir: &Path, depth: usize, budget: &mut Budget, visited: &mut HashSet<PathBuf>, names: &mut BTreeSet<String>) {
    let Ok(real) = std::fs::canonicalize(dir) else { return };
    if !visited.insert(real.clone()) || budget.stats == 0 {
        return;
    }
    budget.stats -= 1;
    let skill = dir.join("SKILL.md");
    if let Ok(meta) = std::fs::metadata(&skill)
        && meta.is_file()
        && let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(str::trim).filter(|n| !n.is_empty())
    {
        let mut visible = true;
        if meta.len() <= SKILL_BYTES.min(budget.bytes)
            && let Ok(text) = std::fs::read_to_string(&skill)
        {
            budget.bytes -= meta.len();
            visible = cli_visible(&text);
        }
        if visible {
            names.insert(name.to_string());
        }
    }
    // A link that leads out of the root shows its own skill only.
    if !real.starts_with(root) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut children: Vec<_> = entries.filter_map(|e| e.ok()).map(|e| e.file_name()).collect();
    children.sort();
    for child in children {
        if child == "node_modules" {
            continue;
        }
        if budget.stats == 0 {
            return;
        }
        budget.stats -= 1;
        let path = dir.join(&child);
        if !std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) || depth >= SKILL_DEPTH {
            continue;
        }
        if budget.entries == 0 {
            return;
        }
        budget.entries -= 1;
        visit(root, &path, depth + 1, budget, visited, names);
    }
}

/// A skill is offered to the CLI unless its frontmatter's `metadata.surfaces` leaves `cli` out.
fn cli_visible(text: &str) -> bool {
    let Some(front) = text.strip_prefix("---").and_then(|rest| rest.split_once("\n---")).map(|(f, _)| f) else { return true };
    let Ok(yaml) = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(front) else { return false };
    let surfaces: Vec<String> = match &yaml["metadata"]["surfaces"] {
        serde_yaml_ng::Value::Sequence(s) => s.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        serde_yaml_ng::Value::String(s) => s.split(',').map(str::to_string).collect(),
        _ => Vec::new(),
    };
    surfaces.is_empty() || surfaces.iter().any(|s| s.trim().eq_ignore_ascii_case("cli"))
}

/// `$name` → `/name` for the skills it has (T3's `rewriteCursorSkillMentions`): a currency sign
/// at the start of a word, then a name with a letter in it; never an amount like `$5` or `$2k`.
pub fn rewrite_mentions(text: &str, skills: &BTreeSet<String>) -> String {
    if skills.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut at_start = true;
    while let Some(c) = rest.chars().next() {
        let len = c.len_utf8();
        if at_start && is_currency(c) {
            let word_end = rest[len..].find(char::is_whitespace).map_or(rest.len(), |i| i + len);
            let name = &rest[len..word_end];
            let valid = name.starts_with(|c: char| c.is_ascii_alphanumeric())
                && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '_' | '-'))
                && name.chars().any(|c| c.is_ascii_alphabetic())
                && !amount(name);
            if valid && skills.contains(name) {
                out.push('/');
                out.push_str(name);
                rest = &rest[word_end..];
                at_start = false;
                continue;
            }
        }
        out.push(c);
        at_start = c.is_whitespace();
        rest = &rest[len..];
    }
    out
}

/// Unicode's currency symbols (`\p{Sc}`) that people type.
fn is_currency(c: char) -> bool {
    matches!(c, '$' | '¢' | '£' | '¤' | '¥' | '֏' | '؋' | '৲' | '৳' | '฿' | '₠'..='⃀' | '﹩' | '＄' | '￠' | '￡' | '￥' | '￦')
}

/// `5`, `2k`, `3e5`: an amount, not a skill (T3's lookahead).
fn amount(name: &str) -> bool {
    let digits = name.trim_start_matches(|c: char| c.is_ascii_digit() || c == '_');
    if digits.len() == name.len() || !name.starts_with(|c: char| c.is_ascii_digit()) {
        return false;
    }
    let suffix = digits;
    suffix.is_empty()
        || (suffix.len() == 1 && suffix.chars().all(|c| "kKmMbBtT".contains(c)))
        || ((suffix.starts_with('e') || suffix.starts_with('E')) && suffix.len() > 1 && suffix[1..].chars().all(|c| c.is_ascii_digit()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_proto::OptionChoice;

    #[test]
    fn options_follow_t3s_cursor_mapping() {
        let select = |id: &str, values: &[&str], default: &str| ModelOption::Select {
            id: id.into(),
            label: id.into(),
            description: None,
            choices: values.iter().map(|v| OptionChoice::new(v, v)).collect(),
            default: Some(default.into()),
            role: None,
        };
        let mapped = options(vec![
            select("thinking", &["false", "true"], "true"),
            select("verbosity", &["low", "high"], "low"),
            select("fast", &["false", "true"], "false"),
            select("context", &["200k", "1m"], "200k"),
            ModelOption::reasoning(vec![OptionChoice::new("low", "Low"), OptionChoice::new("high", "High")], Some("high")),
        ]);
        let ids: Vec<&str> = mapped.iter().map(ModelOption::id).collect();
        assert_eq!(ids, ["effort", "context", "fast", "thinking", "verbosity"]);
        assert!(matches!(&mapped[1], ModelOption::Select { role: Some(OptionRole::ContextWindow), .. }));
        assert_eq!(mapped[2], ModelOption::Boolean { id: "fast".into(), label: "fast".into(), description: None, default: false, role: Some(OptionRole::Speed) });
        assert_eq!(mapped[3], ModelOption::Boolean { id: "thinking".into(), label: "thinking".into(), description: None, default: true, role: None });
        assert!(matches!(&mapped[4], ModelOption::Select { role: None, .. }));
        assert_eq!(model("auto", &["default", "gpt-5"]), "default");
        assert_eq!(model("auto", &["auto"]), "auto");
        assert_eq!(model("gpt-5", &["default"]), "gpt-5");
        assert_eq!((context_tokens("200k"), context_tokens("1M"), context_tokens("272k")), (Some(200_000), Some(1_000_000), Some(272_000)));
        assert_eq!((context_tokens("max"), context_tokens("0k"), context_tokens("")), (None, None, None));
    }

    #[test]
    fn extensions_parse_as_cursor_sends_them() {
        let todos = extension("cursor/update_todos", &json!({ "toolCallId": "t", "merge": true, "todos": [
            { "id": "1", "content": "Read", "status": "completed" }, { "id": "2", "content": "Write", "status": "in_progress" },
            { "id": "3", "content": "Dropped", "status": "cancelled" }, { "content": "no id" } ] }));
        let Some(Extension::Todos { todos, merge: true }) = todos else { panic!("{todos:?}") };
        assert_eq!(todos.len(), 2);
        let mut list = Todos::default();
        list.apply(todos, false);
        let after = list.apply(vec![("2".into(), Todo { text: "Write".into(), status: TodoStatus::Completed }), ("4".into(), Todo { text: "Ship".into(), status: TodoStatus::Pending })], true);
        assert_eq!(after.iter().map(|t| (t.text.as_str(), t.status)).collect::<Vec<_>>(), [("Read", TodoStatus::Completed), ("Write", TodoStatus::Completed), ("Ship", TodoStatus::Pending)]);
        assert_eq!(extension("_cursor/task", &json!({ "toolCallId": "t1", "model": "gpt-5" })), Some(Extension::Task { tool: "t1".into(), model: Some("gpt-5".into()) }));
        let plan = extension("cursor/create_plan", &json!({ "toolCallId": "p", "name": "Ship it", "plan": "1. a" })).unwrap();
        assert_eq!(plan, Extension::Plan { tool: Some("p".into()), text: "# Ship it\n\n1. a".into() });
        assert_eq!(acknowledged(&plan), json!({ "outcome": { "outcome": "accepted" } }));
        assert_eq!(extension("cursor/generate_image", &json!({})), None);
        assert_eq!(extension("session/prompt", &json!({})), None);
    }

    #[test]
    fn questions_answer_with_option_ids() {
        let Some(Extension::Ask(ask)) = extension("cursor/ask_question", &json!({ "toolCallId": "q", "title": "Setup", "questions": [
            { "id": "db", "prompt": "Which database?", "options": [{ "id": "pg", "label": "Postgres" }, { "id": "sq", "label": "SQLite" }], "allowMultiple": false },
            { "id": "extras", "prompt": "Extras?", "options": [{ "id": "a", "label": "Auth" }, { "id": "b", "label": "Billing" }], "allowMultiple": true } ] })) else { panic!() };
        assert_eq!(ask.questions[0].header, "Setup");
        assert!(ask.questions[1].multi && !ask.questions[0].multi);
        let answers = BTreeMap::from([("db".to_string(), "SQLite".to_string()), ("extras".to_string(), "Auth, Billing".to_string())]);
        assert_eq!(answered(&ask.options, &answers).unwrap(), json!({ "outcome": { "outcome": "answered", "answers": [
            { "questionId": "db", "selectedOptionIds": ["sq"] }, { "questionId": "extras", "selectedOptionIds": ["a", "b"] }] } }));
        assert!(answered(&ask.options, &BTreeMap::from([("db".to_string(), "Mongo".to_string())])).is_err());
        assert_eq!(unanswered(false)["outcome"]["outcome"], "skipped");
    }

    #[test]
    fn subagents_and_status() {
        let spawned = json!({ "sessionUpdate": "subagent_spawned", "subagentSessionId": "c.1", "name": "explore", "task": "Look around",
            "_meta": { "cursor": { "toolCallId": "task-1", "agentId": "a1", "model": "composer-2" } } });
        assert_eq!(subagent_update(&spawned), Some(("task-1".into(), ToolExtra { model: Some("composer-2".into()), ended: None })));
        let done = json!({ "sessionUpdate": "subagent_state_update", "state": "disconnected", "_meta": { "cursor": { "toolCallId": "task-1" } } });
        assert_eq!(subagent_update(&done).unwrap().1.ended, Some(agent_proto::NodeStatus::Interrupted));
        // `cursor-agent status --format json` as 2026.11.05 prints it.
        let ok = r#"{ "status": "authenticated", "isAuthenticated": true, "hasAccessToken": true, "hasRefreshToken": true, "userInfo": { "email": "a@b.c" } }"#;
        assert_eq!(auth(ok), (AuthStatus::Ready, Some("Logged in as a@b.c".into())));
        assert_eq!(auth(r#"{ "status": "unauthenticated", "isAuthenticated": false, "message": "Not logged in" }"#).0, AuthStatus::Missing);
        assert_eq!(auth(r#"{ "status": "error", "message": "Status check error: x" }"#).0, AuthStatus::Unknown);
        assert_eq!(auth("not json").0, AuthStatus::Unknown);
        assert!(expected("cursor", Path::new("x")) && expected("mine", Path::new("/opt/cursor-agent")) && !expected("mine", Path::new("agent")));
    }

    #[test]
    fn skills_are_found_within_bounds_and_mentions_rewritten() {
        let dir = tempfile::tempdir().unwrap();
        let (cwd, home, outside) = (dir.path().join("ws"), dir.path().join("home"), dir.path().join("outside"));
        let skill = |base: &Path, rel: &str, text: &str| {
            std::fs::create_dir_all(base.join(rel)).unwrap();
            std::fs::write(base.join(rel).join("SKILL.md"), text).unwrap();
        };
        skill(&cwd, ".cursor/skills/review", "---\nname: Review\ndescription: Reviews\n---\nbody");
        skill(&cwd, ".agents/skills/group/deploy", "no frontmatter");
        skill(&cwd, ".claude/skills/web-only", "---\nmetadata:\n  surfaces: [web]\n---\n");
        skill(&cwd, ".claude/skills/node_modules/dep", "");
        skill(&home, ".codex/skills/notes", "---\nmetadata:\n  surfaces: cli,web\n---\n");
        skill(&home, ".cursor/skills-cursor/builtin", "");
        // A link out of a root shows its own skill but isn't walked.
        skill(&outside, "linked", "");
        skill(&outside, "linked/inner", "");
        std::os::unix::fs::symlink(outside.join("linked"), cwd.join(".cursor/skills/linked")).unwrap();
        let names = skill_names(&cwd, Some(&home));
        assert_eq!(names.iter().map(String::as_str).collect::<Vec<_>>(), ["builtin", "deploy", "linked", "notes", "review"]);

        let text = "Use $review then $deploy, pay $5 or $2k, keep $unknown and a$review; €notes too\n$review";
        assert_eq!(rewrite_mentions(text, &names), "Use /review then $deploy, pay $5 or $2k, keep $unknown and a$review; /notes too\n/review");
        assert_eq!(rewrite_mentions("$review", &BTreeSet::new()), "$review");
    }
}
