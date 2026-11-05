//! The slice of the Agent Client Protocol the adapter reads, parsed tolerantly from JSON.
//!
//! Agents ship different revisions of the schema (v1, and v2 previews whose responses have an
//! `info` instead of an `agentInfo`), and some send fields the schema doesn't know or leave out
//! ones it requires. Like T3's `effect-acp/src/client.ts`, which normalizes v2 shapes onto v1
//! ones, everything here is read field by field: a missing or mistyped field is absent, never a
//! failed session.

use serde_json::Value;

/// Which generation of the protocol the agent answered `initialize` with. Antigravity reports
/// `protocolVersion: 2` with v1 shapes, so the shape decides (T3: `isV2InitializeResponse`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Generation {
    V1,
    V2,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Initialize {
    pub generation: Generation,
    pub protocol_version: u64,
    pub name: String,
    pub title: Option<String>,
    pub version: Option<String>,
    /// `session/load` (v1) or `session/resume` with a replay (v2).
    pub load_session: bool,
    /// `session/resume` without a replay (v1 `sessionCapabilities.resume`).
    pub resume_session: bool,
    pub close_session: bool,
    pub images: bool,
    pub mcp_http: bool,
    pub mcp_sse: bool,
    pub auth_methods: Vec<AuthMethod>,
    /// Models advertised before any session (Grok: `_meta.modelState`).
    pub models: Option<Models>,
    /// Which agent with quirks of its own this is, if any.
    pub flavor: Flavor,
}

/// The agents with quirks of their own, told apart by what `initialize` says.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Flavor {
    #[default]
    Generic,
    /// Kiro (it sends no `agentInfo`, but its capabilities carry `_meta.kiro`).
    Kiro,
    /// Google Antigravity (`agentInfo.name` `antigravity-acp`; `antigravity.rs`).
    Antigravity,
    /// Cursor (`cursor-agent acp`; no `agentInfo`, but its `cursor_login` method; `cursor.rs`).
    Cursor,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthMethod {
    pub id: String,
    pub name: String,
}

pub fn initialize(v: &Value) -> Initialize {
    let v2 = v.get("info").is_some_and(Value::is_object);
    let generation = if v2 { Generation::V2 } else { Generation::V1 };
    let info = if v2 { &v["info"] } else { &v["agentInfo"] };
    let auth_methods = array(&v["authMethods"])
        .iter()
        .filter_map(|m| {
            let id = text(&m["id"]).or_else(|| text(&m["methodId"]))?;
            Some(AuthMethod { name: text(&m["name"]).unwrap_or_else(|| id.clone()), id })
        })
        .collect();
    let mut init = Initialize {
        generation,
        protocol_version: v["protocolVersion"].as_u64().unwrap_or(1),
        name: text(&info["name"]).unwrap_or_default(),
        title: text(&info["title"]),
        version: text(&info["version"]),
        load_session: false,
        resume_session: false,
        close_session: false,
        images: false,
        mcp_http: false,
        mcp_sse: false,
        auth_methods,
        models: models(&v["_meta"]["modelState"]),
        flavor: Flavor::Generic,
    };
    if init.name == super::antigravity::AGENT_NAME {
        init.flavor = Flavor::Antigravity;
    } else if init.auth_methods.iter().any(|m| m.id == super::cursor::AUTH_METHOD) || init.name.to_lowercase().contains("cursor") {
        init.flavor = Flavor::Cursor;
    } else if init.name.to_lowercase().contains("kiro") || v["agentCapabilities"]["_meta"]["kiro"].is_object() {
        init.flavor = Flavor::Kiro;
    }
    if v2 {
        // v2: the presence of a capability object is the capability.
        let session = &v["capabilities"]["session"];
        let present = |x: &Value| !x.is_null();
        init.load_session = session.is_object();
        init.close_session = session.is_object();
        init.images = present(&session["prompt"]["image"]);
        init.mcp_http = present(&session["mcp"]["http"]);
        init.mcp_sse = present(&session["mcp"]["sse"]);
    } else {
        let caps = &v["agentCapabilities"];
        let flag = |x: &Value| x.as_bool().unwrap_or(false) || x.is_object();
        init.load_session = caps["loadSession"].as_bool().unwrap_or(false);
        init.resume_session = caps["sessionCapabilities"]["resume"].is_object();
        init.close_session = caps["sessionCapabilities"]["close"].is_object();
        init.images = flag(&caps["promptCapabilities"]["image"]);
        init.mcp_http = flag(&caps["mcpCapabilities"]["http"]);
        init.mcp_sse = flag(&caps["mcpCapabilities"]["sse"]);
    }
    init
}

/// What `session/new`, `session/load` and `session/resume` answer with.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Setup {
    pub session_id: Option<String>,
    /// Legacy session modes (`session/set_mode`).
    pub modes: Option<Modes>,
    /// Unstable session models (`session/set_model`).
    pub models: Option<Models>,
    pub config: Option<Vec<ConfigOption>>,
}

pub fn setup(v: &Value) -> Setup {
    Setup {
        session_id: text(&v["sessionId"]),
        modes: modes(&v["modes"]),
        models: models(&v["models"]),
        config: v.get("configOptions").filter(|c| c.is_array()).map(config_options),
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Modes {
    pub current: String,
    pub available: Vec<Choice>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Models {
    pub current: Option<String>,
    pub available: Vec<Choice>,
}

/// Something an agent offers: a mode, a model or a config option's value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub id: String,
    pub name: String,
}

pub fn modes(v: &Value) -> Option<Modes> {
    let current = text(&v["currentModeId"])?;
    let available = array(&v["availableModes"]).iter().filter_map(|m| choice(&m["id"], &m["name"])).collect();
    Some(Modes { current, available })
}

pub fn models(v: &Value) -> Option<Models> {
    if !v.is_object() {
        return None;
    }
    let available: Vec<Choice> =
        array(&v["availableModels"]).iter().filter_map(|m| choice(&m["modelId"], &m["name"])).collect();
    let current = text(&v["currentModelId"]);
    (current.is_some() || !available.is_empty()).then_some(Models { current, available })
}

fn choice(id: &Value, name: &Value) -> Option<Choice> {
    let id = text(id)?;
    Some(Choice { name: text(name).unwrap_or_else(|| id.clone()), id })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    pub category: Option<String>,
    pub value: ConfigValue,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ConfigValue {
    Select { current: String, values: Vec<Choice> },
    Boolean(bool),
}

impl ConfigOption {
    pub fn current(&self) -> Option<&str> {
        match &self.value {
            ConfigValue::Select { current, .. } => Some(current),
            ConfigValue::Boolean(_) => None,
        }
    }

    pub fn offers(&self, value: &str) -> bool {
        match &self.value {
            ConfigValue::Select { values, .. } => values.iter().any(|c| c.id == value),
            ConfigValue::Boolean(_) => false,
        }
    }

    pub fn values(&self) -> &[Choice] {
        match &self.value {
            ConfigValue::Select { values, .. } => values,
            ConfigValue::Boolean(_) => &[],
        }
    }
}

/// `configOptions`: v1 names the option `id`, v2 `configId`; select values may be grouped.
/// Options that don't parse are dropped (T3: `normalizeConfigOptions`).
pub fn config_options(v: &Value) -> Vec<ConfigOption> {
    array(v).iter().filter_map(config_option).collect()
}

fn config_option(v: &Value) -> Option<ConfigOption> {
    let id = text(&v["id"]).or_else(|| text(&v["configId"]))?;
    let value = match v["type"].as_str()? {
        "select" => {
            let mut values = Vec::new();
            for entry in array(&v["options"]) {
                match entry.get("options") {
                    Some(group) => values.extend(array(group).iter().filter_map(|o| choice(&o["value"], &o["name"]))),
                    None => values.extend(choice(&entry["value"], &entry["name"])),
                }
            }
            ConfigValue::Select { current: text(&v["currentValue"]).unwrap_or_default(), values }
        }
        "boolean" => ConfigValue::Boolean(v["currentValue"].as_bool()?),
        _ => return None,
    };
    Some(ConfigOption { name: text(&v["name"]).unwrap_or_else(|| id.clone()), id, category: text(&v["category"]), value })
}

/// An option of a permission request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionOption {
    pub id: String,
    pub kind: String,
}

pub fn permission_options(v: &Value) -> Vec<PermissionOption> {
    array(v)
        .iter()
        .filter_map(|o| Some(PermissionOption { id: text(&o["optionId"])?, kind: text(&o["kind"]).unwrap_or_default() }))
        .collect()
}

/// A non-empty, trimmed string.
pub fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

/// A file path; Kiro sends `file://` URIs where ACP says paths.
pub fn path(v: &Value) -> Option<String> {
    let path = text(v)?;
    Some(path.strip_prefix("file://").map(str::to_string).unwrap_or(path))
}

pub fn array(v: &Value) -> &[Value] {
    v.as_array().map(Vec::as_slice).unwrap_or_default()
}

/// The text a content block shows (T3: `acpContentBlockDisplayText`).
pub fn content_text(block: &Value) -> Option<String> {
    match block["type"].as_str()? {
        "text" => block["text"].as_str().map(str::to_string),
        "resource_link" => {
            let uri = block["uri"].as_str()?;
            let name = block["title"].as_str().or(block["name"].as_str()).unwrap_or(uri);
            Some(format!("[{name}]({uri})"))
        }
        "resource" => {
            let resource = &block["resource"];
            resource["text"].as_str().map(str::to_string).or_else(|| resource["uri"].as_str().map(|u| format!("[{u}]({u})")))
        }
        "image" => Some("[image]".to_string()),
        "audio" => Some("[audio]".to_string()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_v1_and_v2_shapes() {
        // T3's `effect-acp/test/fixtures/antigravity-initialize.json`: version 2, v1 shapes.
        let antigravity: Value =
            serde_json::from_str(include_str!("../../../tests/fixtures/acp/antigravity-initialize.json")).unwrap();
        let init = initialize(&antigravity);
        assert_eq!(init.generation, Generation::V1);
        assert_eq!(init.protocol_version, 2);
        assert_eq!(init.name, "antigravity-acp");
        assert_eq!(init.flavor, Flavor::Antigravity);
        assert_eq!(init.version.as_deref(), Some("agy_acp_server_1.1.1"));
        assert!(init.load_session && init.resume_session && !init.close_session);
        assert!(init.images && init.mcp_http);
        assert_eq!(init.auth_methods.len(), 4);
        assert_eq!(init.auth_methods[0], AuthMethod { id: "oauth-personal".into(), name: "Log in with Google".into() });

        let v2 = initialize(&json!({
            "protocolVersion": 2,
            "info": { "name": "mock-agent", "version": "0.0.0" },
            "capabilities": { "session": { "prompt": { "image": {} }, "mcp": { "http": {} } } },
            "authMethods": [{ "type": "env_var", "methodId": "api_key", "name": "API key" }],
        }));
        assert_eq!(v2.generation, Generation::V2);
        assert!(v2.load_session && v2.close_session && v2.images && v2.mcp_http && !v2.mcp_sse);
        assert_eq!(v2.auth_methods[0].id, "api_key");

        // Nothing at all is still an agent, with nothing advertised.
        let bare = initialize(&json!({}));
        assert_eq!(bare.generation, Generation::V1);
        assert!(!bare.load_session && !bare.images);

        // cursor-agent 2026.11.05 sends no agentInfo: its sign-in method says it's Cursor.
        let cursor: Value = serde_json::from_str(include_str!("../../../tests/fixtures/acp/cursor-initialize.json")).unwrap();
        let cursor = initialize(&cursor);
        assert_eq!((cursor.flavor, cursor.name.as_str()), (Flavor::Cursor, ""));
        assert!(cursor.load_session && cursor.images && cursor.mcp_http && !cursor.resume_session);
    }

    #[test]
    fn config_options_tolerate_both_generations_and_junk() {
        let options = config_options(&json!([
            { "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "vibe",
              "options": [{ "value": "vibe", "name": "Default" }, { "group": "g", "name": "G", "options": [{ "value": "plan", "name": "Plan" }] }] },
            { "configId": "autopilot", "name": "Autopilot", "type": "boolean", "currentValue": true },
            { "id": "broken", "type": "select" },
            { "id": "future", "type": "slider", "currentValue": 3 },
            "not an object",
        ]));
        assert_eq!(options.len(), 3);
        assert!(options[0].offers("plan") && options[0].offers("vibe"));
        assert_eq!(options[0].current(), Some("vibe"));
        assert_eq!(options[1].value, ConfigValue::Boolean(true));
        assert_eq!(options[2].current(), Some(""));
    }

    #[test]
    fn content_blocks_render_as_text() {
        assert_eq!(content_text(&json!({ "type": "text", "text": "hi" })).as_deref(), Some("hi"));
        assert_eq!(content_text(&json!({ "type": "resource_link", "uri": "file:///a", "name": "a" })).as_deref(), Some("[a](file:///a)"));
        assert_eq!(content_text(&json!({ "type": "chart", "points": [] })), None);
    }
}
