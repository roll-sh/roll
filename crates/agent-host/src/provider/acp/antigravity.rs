//! Google Antigravity (`agy_acp_server`, Google's own ACP server): what it needs beyond the
//! protocol. Everything here is pure; the hooks that use it live in `acp.rs`, `session.rs`,
//! `translate.rs` and `policy.rs`, behind [`Flavor::Antigravity`](super::wire::Flavor).
//!
//! Ported from T3 Code (MIT), `apps/server/src/`:
//! - `provider/acp/AntigravityAcpSupport.ts`: the eager `authenticate`, `session/resume` over
//!   `session/load`, client capabilities (reads only: advertising writes turns its diff edits
//!   into whole-file writes; no terminals), the session `_meta` (`run_workflow` off on 1.4.2 to
//!   1.4.5, the system prompt appended on 1.4.5) and the permission modes.
//! - `provider/acp/AntigravityProtocol.ts`: `interaction_*` permission requests as single-choice
//!   questions, the `agy.security.warning` on "allow always", tool payloads bounded (8,000
//!   characters kept from the end, images and `formatted_output` dropped), the subagent tools by
//!   title, and the repair of `client_view_file` reads the agent never completes.
//! - `provider/acp/AntigravityChildWork.ts`: a subagent's launch from its permission prompt.
//! - `provider/acp/AntigravityTurnOutcome.ts`: `stopReason: "refusal"` is the quota.
//! - `provider/antigravityAuthSupport.ts`: the sign-in link it prints on stdout, and
//!   `isAntigravitySignInRequiredError`.
//! - `provider/AntigravityInstallation.ts` and `Drivers/AntigravityDriver.ts`: the binary and its
//!   `localharness_external` sibling, and a status check that never starts it (it unpacks about
//!   1 GB each launch).
//! - `orchestration-v2/Adapters/AntigravityAdapterV2.ts`: Gemini usage (input with cached reads,
//!   output with thoughts), subagents running until the turn ends.
//!
//! Left out (docs/code/agents.md, "Providers"): T3's own profile directory and credential
//! storage (`GEMINI_HOME`, `AGY_ACP_FORCE_FILE_STORAGE`), its OAuth redirect relay, its quota
//! reader, the reads of the agent's conversation database, edit hunks rebuilt from the file,
//! attachments other than images, and naming itself Zed.

use agent_proto::{NodeBody, NodeStatus, Question, QuestionOption, RuntimeMode, Usage};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};

/// `agentInfo.name` in its `initialize` answer.
pub const AGENT_NAME: &str = "antigravity-acp";
/// The `[[acp]]` name roll knows it by.
pub const NAME: &str = "antigravity";
pub const BINARY: &str = "agy_acp_server.par";
/// Its arguments (T3's `buildAntigravityAcpSpawnInput`: an empty `--uid` on Linux).
pub const ARGS: &[&str] = if cfg!(target_os = "linux") { &["--uid="] } else { &[] };
/// The harness the server runs, shipped beside it.
pub const HARNESS: &str = "localharness_external";

/// What starting it tells the user when it isn't signed in. roll never signs in for anyone.
pub const SIGN_IN: &str = "Google Antigravity isn't signed in: sign in once in a terminal on this computer \
(docs/code/agents.md, \"Providers\"), then start the thread again";
/// What a quota stop says (T3's `turnFailure`).
pub const QUOTA: &str = "You have reached your Antigravity quota for this period.";
/// What a missing install says.
pub const INSTALL: &str = "get Google's Antigravity ACP server from the ACP registry (antigravity-acp) and put \
agy_acp_server.par and localharness_external in one directory on PATH";

/// The line it prints on stdout (not JSON) when `authenticate` starts a browser sign-in.
const SIGN_IN_PREFIX: &[u8] = b"Open the following link to authenticate the ACP server: ";

/// How much of a tool's text is kept (T3's `TOOL_TEXT_LIMIT`), from the end.
pub const TEXT_LIMIT: usize = 8_000;
const TRUNCATED: &str = "[Earlier output truncated]\n\n";
const LABEL_LIMIT: usize = 512;
const WARNING_LIMIT: usize = 512;
/// What a bounded payload may hold (T3's `sanitizeAntigravityToolPayload`).
const PAYLOAD_NODES: usize = 512;
const PAYLOAD_TEXT: usize = 64_000;
const PAYLOAD_DEPTH: usize = 12;
/// Reads remembered for the repair; older ones are forgotten.
const READS_KEPT: usize = 256;

/// T3's title for a subagent batch whose launch it never saw.
pub const BATCH: &str = "Antigravity subagent batch";
const SUBAGENT_TITLES: [&str; 4] = ["Running start_subagent", "Run start_subagent?", "Running invoke_subagent", "Run invoke_subagent?"];
const CLIENT_READ_TITLES: [&str; 2] = ["Running client_view_file", "Run client_view_file?"];

/// Whether the agent configured as `name` running `binary` is Antigravity, known before it
/// answers `initialize` (its client capabilities go out in that request).
pub fn expected(name: &str, binary: &Path) -> bool {
    name == NAME || binary.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("agy_acp_server"))
}

/// The harness beside the real binary (T3's `fromExternal`: the binary's real path's directory).
pub fn harness(binary: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(super::which(binary)?).ok()?;
    Some(real.parent()?.join(HARNESS))
}

/// The environment it starts with, besides the configured one: the harness path and unbuffered
/// Python output (T3's `buildAntigravityAcpSpawnInput`). A variable the config sets wins.
pub fn spawn_env(binary: &Path, configured: &[(String, String)]) -> Vec<(String, String)> {
    let set = |k: &str| configured.iter().any(|(name, _)| name == k);
    let mut env = Vec::new();
    if !set("ANTIGRAVITY_HARNESS_PATH")
        && let Some(harness) = harness(binary)
    {
        env.push(("ANTIGRAVITY_HARNESS_PATH".to_string(), harness.display().to_string()));
    }
    if !set("PYTHONUNBUFFERED") {
        env.push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));
    }
    env
}

/// `clientCapabilities`: reads, no writes (they switch it to whole-file edits), no terminals.
pub fn client_capabilities() -> Value {
    json!({ "fs": { "readTextFile": true, "writeTextFile": false }, "terminal": false })
}

/// The method `authenticate` names: Google sign-in when offered (T3's default), else the first.
/// Sending it reads no credential: the agent signs in from its own store, or says it can't.
pub fn auth_method(offered: &[super::wire::AuthMethod]) -> String {
    const PERSONAL: &str = "oauth-personal";
    if offered.iter().any(|m| m.id == PERSONAL) {
        return PERSONAL.to_string();
    }
    offered.first().map(|m| m.id.clone()).unwrap_or_else(|| PERSONAL.to_string())
}

/// `1.4.5` out of its version string (`1.4.5`, `agy_acp_server_1.4.5`).
fn version(text: Option<&str>) -> Option<[u64; 3]> {
    let text = text?;
    text.split(|c: char| !(c.is_ascii_digit() || c == '.')).find_map(|word| {
        let parts: Vec<u64> = word.split('.').map(str::parse).collect::<Result<_, _>>().ok()?;
        match parts.as_slice() {
            [a, b, c, ..] => Some([*a, *b, *c]),
            _ => None,
        }
    })
}

/// 1.4.5 appends `_meta.antigravity.systemPrompt.append` on every new, load and resume.
pub fn appends_system_prompt(agent_version: Option<&str>) -> bool {
    version(agent_version) == Some([1, 4, 5])
}

/// The session `_meta` for `session/new` and `session/resume`: 1.4.2 to 1.4.5 write a
/// `run_workflow` skill into the workspace unless the tool is off; 1.4.5 takes instructions.
pub fn session_meta(agent_version: Option<&str>, instructions: Option<&str>) -> Option<Value> {
    let mut antigravity = Map::new();
    if version(agent_version).is_some_and(|v| ([1, 4, 2]..=[1, 4, 5]).contains(&v)) {
        antigravity.insert("disabledTools".into(), json!(["run_workflow"]));
    }
    if let Some(append) = instructions.filter(|_| appends_system_prompt(agent_version)) {
        antigravity.insert("systemPrompt".into(), json!({ "append": append }));
    }
    (!antigravity.is_empty()).then(|| json!({ "antigravity": antigravity }))
}

/// Its session mode for roll's runtime mode (T3's `antigravityPermissionMode`).
pub fn permission_mode(runtime: RuntimeMode) -> &'static str {
    match runtime {
        RuntimeMode::FullAccess => "yolo",
        RuntimeMode::AutoEdit => "auto_edit",
        RuntimeMode::Ask => "default",
    }
}

/// A stdout line that isn't JSON: its sign-in link means it waits for a browser sign-in roll
/// won't relay, so the connection ends with [`SIGN_IN`] (the link isn't repeated anywhere).
pub fn stdout_line(line: &[u8]) -> Option<String> {
    line.starts_with(SIGN_IN_PREFIX).then(|| SIGN_IN.to_string())
}

/// Gemini usage: `inputTokens` leaves out cached reads and `outputTokens` thoughts.
pub fn usage(u: &Value) -> Usage {
    let n = |k: &str| u[k].as_u64().unwrap_or(0);
    let input = n("inputTokens").saturating_add(n("cachedReadTokens"));
    Usage {
        input_tokens: input,
        output_tokens: n("outputTokens").saturating_add(n("thoughtTokens")),
        cached_tokens: n("cachedReadTokens").min(input),
        cost_usd: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Text bounds
// ---------------------------------------------------------------------------------------------

/// The last `limit` characters, marked when cut (T3's `boundText`).
pub fn bound(text: &str, limit: usize) -> String {
    let count = text.chars().count();
    if count <= limit {
        return text.to_string();
    }
    let start = text.char_indices().nth(count - limit).map_or(0, |(i, _)| i);
    format!("{TRUNCATED}{}", &text[start..])
}

/// The first `limit` characters, `...` when cut (labels and questions).
fn head(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit.saturating_sub(3)).collect();
    format!("{kept}...")
}

struct Budget {
    nodes: usize,
    text: usize,
}

/// A tool payload with its strings bounded, inline images and `formatted_output` dropped, and
/// at most [`PAYLOAD_NODES`] values (T3's `sanitizeToolValue`).
pub fn sanitize(v: &Value) -> Value {
    sanitize_value(v, &mut Budget { nodes: PAYLOAD_NODES, text: PAYLOAD_TEXT }, 0).unwrap_or(Value::Null)
}

fn sanitize_value(v: &Value, budget: &mut Budget, depth: usize) -> Option<Value> {
    if depth > PAYLOAD_DEPTH || budget.nodes == 0 {
        return None;
    }
    budget.nodes -= 1;
    match v {
        Value::String(s) => {
            if s.get(..11).is_some_and(|p| p.eq_ignore_ascii_case("data:image/")) || budget.text == 0 {
                return None;
            }
            let text = bound(s, TEXT_LIMIT.min(budget.text));
            budget.text = budget.text.saturating_sub(text.chars().count());
            Some(Value::String(text))
        }
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                if budget.nodes == 0 {
                    break;
                }
                out.extend(sanitize_value(item, budget, depth + 1));
            }
            Some(Value::Array(out))
        }
        Value::Object(map) => {
            let image = map.get("type").and_then(Value::as_str) == Some("image")
                || map.get("mimeType").and_then(Value::as_str).is_some_and(|m| m.starts_with("image/"));
            let mut out = Map::new();
            for (key, value) in map {
                if budget.nodes == 0 {
                    break;
                }
                if (image && matches!(key.as_str(), "data" | "blob")) || matches!(key.as_str(), "formatted_output" | "formattedOutput") {
                    continue;
                }
                if let Some(value) = sanitize_value(value, budget, depth + 1) {
                    out.insert(key.clone(), value);
                }
            }
            Some(Value::Object(out))
        }
        other => Some(other.clone()),
    }
}

/// A `tool_call`/`tool_call_update` with its payloads bounded (T3's
/// `normalizeAntigravitySessionUpdate`); content that isn't text or a diff is dropped.
pub fn sanitize_update(update: &Value) -> Value {
    let mut out = update.clone();
    for key in ["rawInput", "rawOutput", "_meta"] {
        if let Some(v) = update.get(key).filter(|v| !v.is_null()) {
            out[key] = sanitize(v);
        }
    }
    if let Some(title) = update["title"].as_str() {
        out["title"] = json!(bound(title, TEXT_LIMIT));
    }
    if let Some(content) = update["content"].as_array() {
        let mut budget = Budget { nodes: PAYLOAD_NODES, text: 32_000 };
        let kept: Vec<Value> = content
            .iter()
            .filter_map(|entry| sanitize_value(entry, &mut budget, 0))
            .filter(|entry| match entry["type"].as_str() {
                Some("content") => entry["content"]["type"] != "image" && entry["content"].is_object(),
                Some("diff") => entry["path"].is_string(),
                Some("terminal") => true,
                _ => false,
            })
            .collect();
        out["content"] = Value::Array(kept);
    }
    out
}

// ---------------------------------------------------------------------------------------------
// Permission requests
// ---------------------------------------------------------------------------------------------

/// Its questions share `session/request_permission`; their tool call ids say which they are.
pub fn is_question(params: &Value) -> bool {
    params["toolCall"]["toolCallId"].as_str().is_some_and(|id| id.starts_with("interaction_"))
}

/// A question: what the user sees, and each choice's option id with its label.
#[derive(Clone, Debug, PartialEq)]
pub struct Choice {
    pub question: Question,
    pub options: Vec<(String, String)>,
}

/// A question as one single choice of the agent's own options (T3's
/// `extractAntigravityUserInputQuestion`). `None` when an option id is missing or repeated.
pub fn question(params: &Value) -> Option<Choice> {
    let call = &params["toolCall"];
    let id = call["toolCallId"].as_str()?.to_string();
    let mut options: Vec<(String, String)> = Vec::new();
    for option in super::wire::array(&params["options"]) {
        let option_id = option["optionId"].as_str().filter(|o| !o.trim().is_empty())?.to_string();
        if options.iter().any(|(o, _)| *o == option_id) {
            return None;
        }
        let name = option["name"].as_str().map(str::trim).filter(|n| !n.is_empty()).unwrap_or(&option_id);
        let mut label = head(name, LABEL_LIMIT);
        // The answer comes back as the label: keep labels apart.
        if options.iter().any(|(_, l)| *l == label) {
            label = format!("{label} ({option_id})");
        }
        options.push((option_id, label));
    }
    if options.is_empty() {
        return None;
    }
    let text = call["title"].as_str().map(str::trim).filter(|t| !t.is_empty()).unwrap_or("Choose an option.");
    let question = Question {
        id,
        header: "Question".into(),
        question: head(text, TEXT_LIMIT),
        options: options.iter().map(|(_, label)| QuestionOption { label: label.clone(), description: None }).collect(),
        multi: false,
    };
    Some(Choice { question, options })
}

/// The option an answer picks: an option id, else exactly one label (T3's
/// `makeAntigravityUserInputResponse`). A free-text answer picks nothing.
pub fn answer<'a>(options: &'a [(String, String)], answer: &str) -> Option<&'a str> {
    if let Some((id, _)) = options.iter().find(|(id, _)| id == answer) {
        return Some(id);
    }
    let mut labelled = options.iter().filter(|(_, label)| label == answer.trim());
    match (labelled.next(), labelled.next()) {
        (Some((id, _)), None) => Some(id),
        _ => None,
    }
}

/// The warning it puts on "allow always" for shell and web tools (prompt injection could rerun
/// the action without asking): the approval's reason.
pub fn security_warning(options: &Value) -> Option<String> {
    let always = super::wire::array(options).iter().find(|o| o["kind"] == "allow_always" && o["optionId"].as_str().is_some_and(|i| !i.trim().is_empty()))?;
    let warning = &always["_meta"]["agy.security.warning"];
    let text = super::wire::text(&warning["message"]).or_else(|| super::wire::text(&warning["title"]))?;
    Some(head(&text, WARNING_LIMIT))
}

// ---------------------------------------------------------------------------------------------
// Subagents
// ---------------------------------------------------------------------------------------------

/// What a tool call carries for Antigravity beyond ACP's fields.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ToolExtra {
    /// The subagents it starts, from its permission prompt (later updates leave them out).
    pub launch: Option<Launch>,
    /// `_meta.is_mcp_tool_call`: an MCP tool with a subagent's title.
    pub mcp: bool,
}

impl ToolExtra {
    /// Take what an update (or a permission request's tool call) adds.
    pub fn merge(&mut self, update: &Value) {
        if let Some(launch) = launch(&update["rawInput"]) {
            self.launch = Some(launch);
        }
        if update["_meta"]["is_mcp_tool_call"] == json!(true) {
            self.mcp = true;
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Launch {
    pub title: String,
    pub prompt: String,
}

/// `invoke_subagent`'s `Subagents: [{TypeName, Role, Prompt}]` (T3's
/// `antigravitySubagentLaunches` and `describeAntigravitySubagentLaunches`).
pub fn launch(input: &Value) -> Option<Launch> {
    let entries = input["Subagents"].as_array().filter(|e| !e.is_empty())?;
    let launches: Vec<(&str, &str)> =
        entries.iter().map(|e| Some((e["Role"].as_str()?, e["Prompt"].as_str()?))).collect::<Option<_>>()?;
    Some(match launches.as_slice() {
        [(role, prompt)] => Launch { title: role.to_string(), prompt: prompt.to_string() },
        many => Launch {
            title: many.iter().map(|(role, _)| *role).collect::<Vec<_>>().join(", "),
            prompt: many.iter().map(|(role, prompt)| format!("{role}\n{prompt}")).collect::<Vec<_>>().join("\n\n"),
        },
    })
}

/// A subagent tool as roll shows it: the batch it started, running until the turn ends (it
/// reports no child sessions, so nothing says when they finish). `None` for any other tool.
pub fn subagent(kind: Option<&str>, title: Option<&str>, extra: &ToolExtra, output: Option<&str>, status: NodeStatus) -> Option<(NodeBody, NodeStatus)> {
    if kind.is_some_and(|k| k != "other") || !title.is_some_and(|t| SUBAGENT_TITLES.contains(&t)) || extra.mcp {
        return None;
    }
    let output = output.map(str::trim).filter(|o| !o.is_empty()).map(|o| bound(o, TEXT_LIMIT));
    let prompt = extra.launch.as_ref().map(|l| l.prompt.clone()).or_else(|| output.clone()).or_else(|| title.map(str::to_string)).unwrap_or_default();
    let agent = extra.launch.as_ref().map_or_else(|| BATCH.to_string(), |l| l.title.clone());
    let (status, result) = match status {
        NodeStatus::Failed => (NodeStatus::Failed, output),
        NodeStatus::Interrupted => (NodeStatus::Interrupted, None),
        _ => (NodeStatus::Running, None),
    };
    Some((NodeBody::Subagent { agent, prompt, child: None, result, progress: None, model: None }, status))
}

// ---------------------------------------------------------------------------------------------
// Served reads
// ---------------------------------------------------------------------------------------------

/// The `client_view_file` repair (T3's `makeAntigravityReadRepair`): the agent never completes
/// some of these calls, then fails every open one with the next unrelated error. A read roll
/// served through `fs/read_text_file` is shown completed.
#[derive(Debug, Default)]
pub struct Reads {
    /// Announced reads: tool call id, path, and whether roll served it (once it tried).
    open: Vec<(String, String, Option<bool>)>,
    /// Reads roll answered before the agent announced them (it runs them in the background).
    early: Vec<(String, bool)>,
}

impl Reads {
    /// A new turn: earlier turns' reads are settled.
    pub fn reset(&mut self) {
        self.open.clear();
        self.early.clear();
    }

    /// roll answered `fs/read_text_file` for `path`: the oldest open read of it takes the
    /// outcome. The tool call a served read completes, to report at once.
    pub fn attempted(&mut self, path: &str, served: bool) -> Option<String> {
        if let Some((id, _, outcome)) = self.open.iter_mut().find(|(_, p, o)| p == path && o.is_none()) {
            *outcome = Some(served);
            return served.then(|| id.clone());
        }
        if self.early.len() == READS_KEPT {
            self.early.remove(0);
        }
        self.early.push((path.to_string(), served));
        None
    }

    /// A tool update as it should read.
    pub fn repair(&mut self, mut update: Value) -> Value {
        let Some(id) = update["toolCallId"].as_str().map(str::to_string) else { return update };
        if let Some(path) = read_path(&update)
            && !self.open.iter().any(|(i, ..)| *i == id)
        {
            let early = self.early.iter().position(|(p, _)| *p == path).map(|i| self.early.remove(i).1);
            if self.open.len() == READS_KEPT {
                self.open.remove(0);
            }
            self.open.push((id.clone(), path, early));
        }
        let at = self.open.iter().position(|(i, ..)| *i == id);
        let served = at.is_some_and(|i| self.open[i].2 == Some(true));
        match update["status"].as_str() {
            Some("pending" | "in_progress") if served => update["status"] = json!("completed"),
            Some(status @ ("completed" | "failed")) => {
                if let Some(i) = at {
                    self.open.remove(i);
                }
                if status == "failed" && served {
                    update["status"] = json!("completed");
                    if let Some(o) = update.as_object_mut() {
                        // The unrelated step error it failed the read with.
                        o.remove("rawOutput");
                    }
                }
            }
            _ => {}
        }
        update
    }
}

/// The native tool a title names: its titles are `run_command`, `Running edit_file` or
/// `Run edit_file?`, never a description.
pub fn tool_name(title: &str) -> Option<&str> {
    let name = title.strip_prefix("Running ").or_else(|| title.strip_prefix("Run ").and_then(|t| t.strip_suffix('?'))).unwrap_or(title);
    (!name.is_empty() && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')).then_some(name)
}

fn read_path(update: &Value) -> Option<String> {
    if !update["title"].as_str().is_some_and(|t| CLIENT_READ_TITLES.contains(&t)) {
        return None;
    }
    super::wire::text(&update["locations"][0]["path"]).or_else(|| super::wire::text(&update["rawInput"]["absolute_path"]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_meta_and_modes() {
        assert_eq!(version(Some("agy_acp_server_1.4.5")), Some([1, 4, 5]));
        assert_eq!(version(Some("1.4.2")), Some([1, 4, 2]));
        assert_eq!(version(Some("nightly")), None);
        assert_eq!(session_meta(Some("agy_acp_server_1.1.1"), Some("be brief")), None);
        assert_eq!(session_meta(Some("1.4.3"), Some("be brief")), Some(json!({ "antigravity": { "disabledTools": ["run_workflow"] } })));
        assert_eq!(
            session_meta(Some("1.4.5"), Some("be brief")),
            Some(json!({ "antigravity": { "disabledTools": ["run_workflow"], "systemPrompt": { "append": "be brief" } } }))
        );
        assert_eq!(session_meta(Some("1.4.6"), None), None);
        assert_eq!(permission_mode(RuntimeMode::FullAccess), "yolo");
        assert_eq!(permission_mode(RuntimeMode::AutoEdit), "auto_edit");
        assert_eq!(permission_mode(RuntimeMode::Ask), "default");
        let method = |ids: &[&str]| auth_method(&ids.iter().map(|id| super::super::wire::AuthMethod { id: id.to_string(), name: id.to_string() }).collect::<Vec<_>>());
        assert_eq!(method(&["gemini-api-key", "oauth-personal"]), "oauth-personal");
        assert_eq!(method(&["gemini-api-key"]), "gemini-api-key");
        assert_eq!(method(&[]), "oauth-personal");
        assert!(stdout_line(b"Open the following link to authenticate the ACP server: https://accounts.google.com/o/oauth2/v2/auth?x").is_some());
        assert!(stdout_line(b"{\"jsonrpc\":\"2.0\"}").is_none());
        assert!(expected("antigravity", Path::new("x")) && expected("mine", Path::new("/opt/agy/agy_acp_server.par")) && !expected("gemini", Path::new("gemini")));
        for (title, name) in [("run_command", Some("run_command")), ("Running client_view_file", Some("client_view_file")), ("Run edit_file?", Some("edit_file")), ("Edit the README", None)] {
            assert_eq!(tool_name(title), name, "{title}");
        }
    }

    #[test]
    fn spawn_env_finds_the_harness_beside_the_real_binary() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("release");
        std::fs::create_dir(&real).unwrap();
        let bin = real.join(BINARY);
        std::fs::write(&bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&bin, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        let link = dir.path().join("agy");
        std::os::unix::fs::symlink(&bin, &link).unwrap();
        let env = spawn_env(&link, &[]);
        let harness = std::fs::canonicalize(&real).unwrap().join(HARNESS);
        assert_eq!(env, [("ANTIGRAVITY_HARNESS_PATH".into(), harness.display().to_string()), ("PYTHONUNBUFFERED".into(), "1".into())]);
        let mine = [("ANTIGRAVITY_HARNESS_PATH".to_string(), "/opt/h".to_string())];
        assert_eq!(spawn_env(&link, &mine), [("PYTHONUNBUFFERED".into(), "1".into())]);
    }

    #[test]
    fn questions_are_single_choices_of_its_own_options() {
        let request = json!({ "sessionId": "s", "toolCall": { "toolCallId": "interaction_9960062f", "status": "pending",
            "title": "Which result label should be used for the verification?", "rawInput": {} },
            "options": [{ "optionId": "1", "name": "Verified", "kind": "allow_once" }, { "optionId": "2", "name": "Needs review", "kind": "allow_once" }] });
        assert!(is_question(&request));
        let choice = question(&request).unwrap();
        assert_eq!(choice.question.id, "interaction_9960062f");
        assert_eq!(choice.question.header, "Question");
        assert!(!choice.question.multi);
        assert_eq!(choice.question.options.iter().map(|o| o.label.as_str()).collect::<Vec<_>>(), ["Verified", "Needs review"]);
        for a in ["1", "Verified"] {
            assert_eq!(answer(&choice.options, a), Some("1"));
        }
        assert_eq!(answer(&choice.options, "arbitrary answer"), None);
        // Repeated labels stay apart; a repeated or empty option id isn't a question.
        let mut same = request.clone();
        same["options"] = json!([{ "optionId": "a", "name": "Same", "kind": "allow_once" }, { "optionId": "b", "name": "Same", "kind": "allow_once" }]);
        let choice = question(&same).unwrap();
        assert_eq!(choice.options[1].1, "Same (b)");
        assert_eq!(answer(&choice.options, "Same (b)"), Some("b"));
        for bad in ["a", ""] {
            same["options"][1]["optionId"] = json!(bad);
            assert!(question(&same).is_none());
        }
        // Long text is bounded; ids never change.
        let mut long = request.clone();
        long["toolCall"]["title"] = json!("Question ".repeat(2_000));
        long["options"] = json!([{ "optionId": "stable-choice", "name": "Choice ".repeat(2_000), "kind": "allow_once" }]);
        let choice = question(&long).unwrap();
        assert!(choice.question.question.chars().count() <= TEXT_LIMIT && choice.options[0].1.chars().count() <= LABEL_LIMIT);
        assert_eq!(choice.options[0].0, "stable-choice");
        assert!(!is_question(&json!({ "toolCall": { "toolCallId": "command-1" } })));
    }

    #[test]
    fn allow_always_carries_its_warning() {
        let options = json!([
            { "optionId": "remember-this-command", "name": "Allow Always (risky)", "kind": "allow_always",
              "_meta": { "agy.security.warning": { "severity": "high", "risk": "prompt_injection", "title": "Allowing always can be risky",
                "message": "Untrusted files could re-run this action without asking." } } },
            { "optionId": "run-this-time", "name": "Allow", "kind": "allow_once" },
        ]);
        assert_eq!(security_warning(&options).as_deref(), Some("Untrusted files could re-run this action without asking."));
        assert_eq!(security_warning(&json!([{ "optionId": "x", "kind": "allow_always" }])), None);
    }

    #[test]
    fn payloads_are_bounded() {
        let output = format!("{}last line\n", "output line\n".repeat(20_000));
        let update = json!({ "toolCallId": "c", "rawOutput": { "commandLine": "cat", "combinedOutput": output, "formatted_output": output },
            "content": [{ "type": "content", "content": { "type": "image", "mimeType": "image/png", "data": "AAAA".repeat(50_000) } },
                        { "type": "content", "content": { "type": "text", "text": "Saved the image." } },
                        { "type": "diff", "path": "/w/note.txt", "oldText": "old", "newText": "new" }] });
        let clean = sanitize_update(&update);
        let text = clean.to_string();
        assert!(text.len() < 10_000, "{}", text.len());
        assert!(!text.contains("formatted_output") && !text.contains("AAAA"));
        assert!(clean["rawOutput"]["combinedOutput"].as_str().unwrap().ends_with("last line\n"));
        assert!(clean["rawOutput"]["combinedOutput"].as_str().unwrap().starts_with(TRUNCATED));
        assert_eq!(clean["content"].as_array().unwrap().len(), 2);
        let nested = sanitize(&json!({ "text": "a".repeat(100_000), "result": { "mimeType": "image/png", "blob": "b" }, "uri": "data:image/png;base64,x" }));
        assert!(nested.to_string().len() < 9_000);
        assert!(nested["result"].get("blob").is_none() && nested.get("uri").is_none());
    }

    #[test]
    fn subagents_by_title_with_their_launch() {
        let extra = ToolExtra { launch: launch(&json!({ "Subagents": [{ "TypeName": "research", "Role": "Readme Reader", "Prompt": "Read README.md" }] })), mcp: false };
        let (body, status) = subagent(Some("other"), Some("Run invoke_subagent?"), &extra, None, NodeStatus::Running).unwrap();
        assert_eq!(status, NodeStatus::Running);
        assert_eq!(body, NodeBody::Subagent { agent: "Readme Reader".into(), prompt: "Read README.md".into(), child: None, result: None, progress: None, model: None });
        // Completed still runs (its children do); its output is the prompt when no launch was seen.
        let (body, status) = subagent(None, Some("Running start_subagent"), &ToolExtra::default(), Some("  Started two agents. "), NodeStatus::Completed).unwrap();
        assert_eq!(status, NodeStatus::Running);
        assert!(matches!(body, NodeBody::Subagent { ref agent, ref prompt, result: None, .. } if agent == BATCH && prompt == "Started two agents."));
        let (_, status) = subagent(None, Some("Running start_subagent"), &ToolExtra::default(), Some("boom"), NodeStatus::Failed).unwrap();
        assert_eq!(status, NodeStatus::Failed);
        for (kind, title, mcp) in [(Some("execute"), "Running start_subagent", false), (None, "Running subagent", false), (None, "Run invoke_subagent?", true)] {
            assert!(subagent(kind, Some(title), &ToolExtra { launch: None, mcp }, None, NodeStatus::Running).is_none());
        }
        let two = launch(&json!({ "Subagents": [{ "Role": "A", "Prompt": "a" }, { "Role": "B", "Prompt": "b" }] })).unwrap();
        assert_eq!(two, Launch { title: "A, B".into(), prompt: "A\na\n\nB\nb".into() });
        assert!(launch(&json!({ "Subagents": [] })).is_none() && launch(&json!({ "Subagents": [{ "Role": "A" }] })).is_none());
    }

    #[test]
    fn served_reads_are_completed() {
        let announce = |reads: &mut Reads, id: &str| {
            reads.repair(json!({ "sessionUpdate": "tool_call", "toolCallId": id, "title": "Running client_view_file", "kind": "read",
                "status": "in_progress", "locations": [{ "path": "/repo/a.ts" }] }))
        };
        let fail = |reads: &mut Reads, id: &str| {
            reads.repair(json!({ "sessionUpdate": "tool_call_update", "toolCallId": id, "status": "failed", "rawOutput": "Tool execution failed" }))
        };
        // Served before it was announced.
        let mut reads = Reads::default();
        assert_eq!(reads.attempted("/repo/a.ts", true), None);
        assert_eq!(announce(&mut reads, "late")["status"], "completed");
        assert_eq!(fail(&mut reads, "late"), json!({ "sessionUpdate": "tool_call_update", "toolCallId": "late", "status": "completed" }));
        // Served after: the read it completes is reported.
        assert_eq!(announce(&mut reads, "now")["status"], "in_progress");
        assert_eq!(reads.attempted("/repo/a.ts", true).as_deref(), Some("now"));
        assert_eq!(fail(&mut reads, "now")["status"], "completed");
        // A refused read fails as it says; an earlier turn's served read doesn't count.
        reads.attempted("/repo/a.ts", true);
        reads.reset();
        announce(&mut reads, "refused");
        reads.attempted("/repo/a.ts", false);
        assert_eq!(fail(&mut reads, "refused")["status"], "failed");
    }

    #[test]
    fn gemini_usage_adds_cached_reads_and_thoughts() {
        let u = usage(&json!({ "inputTokens": 100, "cachedReadTokens": 40, "outputTokens": 10, "thoughtTokens": 5 }));
        assert_eq!((u.input_tokens, u.output_tokens, u.cached_tokens), (140, 15, 40));
        assert_eq!(bound("abcdef", 3), format!("{TRUNCATED}def"));
        assert_eq!(bound("é".repeat(3).as_str(), 3), "ééé");
    }
}
