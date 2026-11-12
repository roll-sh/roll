//! What an ACP agent offers, as roll's model catalog: its models and options from a live
//! session's setup, ported from T3 Code's `acpProviderOptionDescriptors`
//! (`packages/provider-acp/src/server/sessionConfig.ts`, MIT) and its probe's model list
//! (`packages/provider-acp-registry/src/server/probe.ts`).
//!
//! T3 learns these by opening a probe session (`session/new` in its own directory) when it checks
//! a registry agent. roll's probe stops at `initialize` (it never opens a session or signs in as a
//! side effect: an agent may keep every session in its history), so the catalog comes from the
//! user's own sessions instead, T3's `applyAcpRegistryLiveConfiguration`: the first session an
//! agent opens tells the daemon, which keeps it for that agent's version and offers it in the
//! picker from then on.
//!
//! The model is the agent's model option (or v1's session models); its reasoning option (a
//! `thought_level`) is the thread's effort; every other select or switch is an option of the same
//! id, which `policy.rs` writes back. Modes and the model are roll's own controls, so they aren't
//! options. Bounded like T3: 16 options, 64 choices, 256-byte texts.

use super::policy;
use super::wire::{ConfigOption, ConfigValue, Models};
use agent_proto::{ModelInfo, ModelOption, OptionChoice};

const MAX_OPTIONS: usize = 16;
const MAX_CHOICES: usize = 64;
const MAX_TEXT: usize = 256;
/// Models a catalog keeps (an agent may list hundreds of provider models).
const MAX_MODELS: usize = 256;

/// `text` trimmed and cut to [`MAX_TEXT`] bytes on a character boundary.
fn bounded(text: &str) -> String {
    let t = text.trim();
    let mut end = t.len().min(MAX_TEXT);
    while !t.is_char_boundary(end) {
        end -= 1;
    }
    t[..end].to_string()
}

/// An id that is usable as is (T3's `boundedOpaqueValue`): not empty, no edge spaces, short.
fn opaque(id: &str) -> Option<&str> {
    (!id.is_empty() && id == id.trim() && id.len() <= MAX_TEXT && !id.chars().any(char::is_control)).then_some(id)
}

fn choices(option: &ConfigOption) -> Vec<OptionChoice> {
    let mut seen = std::collections::HashSet::new();
    option
        .values()
        .iter()
        .filter_map(|c| opaque(&c.id).filter(|id| seen.insert(id.to_string())).map(|id| OptionChoice::new(id, &bounded(&c.name)).labelled_or(id)))
        .take(MAX_CHOICES)
        .collect()
}

trait LabelledOr {
    fn labelled_or(self, id: &str) -> Self;
}

impl LabelledOr for OptionChoice {
    /// An empty label is the id (T3).
    fn labelled_or(mut self, id: &str) -> Self {
        if self.label.is_empty() {
            self.label = id.to_string();
        }
        self
    }
}

/// The options a session offers besides its model and modes.
pub fn options(config: &[ConfigOption]) -> Vec<ModelOption> {
    let mut out = Vec::new();
    let effort = policy::effort_option(config).map(|o| o.id.clone());
    let reserved = |o: &ConfigOption| matches!(o.category.as_deref(), Some("model" | "mode" | "collaboration_mode")) || o.id == "autopilot" || policy::model_option(config).is_some_and(|m| m.id == o.id);
    let mut seen = std::collections::HashSet::new();
    for option in config {
        if out.len() == MAX_OPTIONS {
            break;
        }
        let Some(id) = opaque(&option.id) else { continue };
        if reserved(option) || !seen.insert(id.to_string()) {
            continue;
        }
        let label = Some(bounded(&option.name)).filter(|l| !l.is_empty()).unwrap_or_else(|| id.to_string());
        match &option.value {
            ConfigValue::Boolean(current) => out.push(ModelOption::Boolean { id: id.into(), label, description: None, default: *current, role: None }),
            ConfigValue::Select { current, .. } => {
                let choices = choices(option);
                if choices.is_empty() {
                    continue;
                }
                let default = choices.iter().any(|c| &c.id == current).then(|| current.clone());
                // The reasoning option rides in a thread's effort (`policy::settings` writes it back).
                if effort.as_deref() == Some(id) {
                    out.insert(0, ModelOption::reasoning(choices, default.as_deref()));
                } else {
                    out.push(ModelOption::Select { id: id.into(), label, description: None, choices, default, role: None });
                }
            }
        }
    }
    out
}

/// The models a session offers, each with the session's options; the current one is the
/// default. Empty when the agent names no models (its default is all there is).
pub fn models(config: &[ConfigOption], legacy: Option<&Models>) -> Vec<ModelInfo> {
    let (list, current): (Vec<(String, String)>, Option<String>) = match policy::model_option(config) {
        Some(option) => (option.values().iter().map(|c| (c.id.clone(), c.name.clone())).collect(), option.current().map(str::to_string)),
        None => match legacy {
            Some(m) => (m.available.iter().map(|c| (c.id.clone(), c.name.clone())).collect(), m.current.clone()),
            None => return Vec::new(),
        },
    };
    let options = options(config);
    let mut seen = std::collections::HashSet::new();
    list.into_iter()
        .filter_map(|(id, name)| {
            let id = opaque(&id)?.to_string();
            seen.insert(id.clone()).then_some(())?;
            let name = Some(bounded(&name)).filter(|n| !n.is_empty()).unwrap_or_else(|| id.clone());
            Some(ModelInfo { default: current.as_deref() == Some(id.as_str()), options: options.clone(), ..ModelInfo::new(id, name) })
        })
        .take(MAX_MODELS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::acp::wire;
    use agent_proto::OptionRole;
    use serde_json::json;

    #[test]
    fn options_follow_t3s_descriptors() {
        let config = wire::config_options(&json!([
            { "id": "model", "name": "Model", "category": "model", "type": "select", "currentValue": "sonnet",
              "options": [{ "value": "sonnet", "name": "Sonnet" }, { "value": "opus", "name": "Opus" }] },
            { "id": "mode", "name": "Mode", "category": "mode", "type": "select", "currentValue": "code", "options": [{ "value": "code", "name": "Code" }, { "value": "plan", "name": "Plan" }] },
            { "id": "thought_level", "name": "Thinking", "category": "thought_level", "type": "select", "currentValue": "medium",
              "options": [{ "value": "low", "name": "Low" }, { "value": "medium", "name": "Medium" }, { "value": "high", "name": "" }] },
            { "configId": "agent", "name": "Agent", "type": "select", "currentValue": "build",
              "options": [{ "group": "x", "name": "Built in", "options": [{ "value": "build", "name": "Build" }, { "value": "plan", "name": "Plan" }, { "value": "build", "name": "Again" }] }] },
            { "id": "web", "name": "Web search", "type": "boolean", "currentValue": true },
            { "id": "autopilot", "name": "Autopilot", "type": "boolean", "currentValue": false },
            { "id": "x".repeat(300), "name": "Too long", "type": "boolean", "currentValue": false },
            { "id": "empty", "name": "Nothing", "type": "select", "currentValue": "", "options": [] },
        ]));
        let options = options(&config);
        let ids: Vec<&str> = options.iter().map(ModelOption::id).collect();
        // The reasoning option first (as the thread's effort), then the agent's own; the model,
        // the mode, Kiro's Autopilot, an id too long to keep and a select with nothing to pick aren't options.
        assert_eq!(ids, ["effort", "agent", "web"]);
        let ModelOption::Select { choices, default, role, .. } = &options[0] else { panic!() };
        assert_eq!((default.as_deref(), *role), (Some("medium"), Some(OptionRole::Reasoning)));
        assert_eq!(choices.iter().map(|c| c.label.as_str()).collect::<Vec<_>>(), ["Low", "Medium", "high"], "an empty label is the id");
        let ModelOption::Select { choices, default, .. } = &options[1] else { panic!() };
        assert_eq!(choices.len(), 2, "a repeated choice is kept once");
        assert_eq!(default.as_deref(), Some("build"));
        assert_eq!(options[2], ModelOption::Boolean { id: "web".into(), label: "Web search".into(), description: None, default: true, role: None });

        let models = models(&config, None);
        assert_eq!(models.iter().map(|m| (m.id.as_str(), m.default)).collect::<Vec<_>>(), [("sonnet", true), ("opus", false)]);
        assert!(models.iter().all(|m| m.options == options));
    }

    #[test]
    fn v1_session_models_and_bounds() {
        let legacy = Models { current: Some("b".into()), available: vec![wire::Choice { id: "a".into(), name: "A".into() }, wire::Choice { id: "b".into(), name: "".into() }] };
        let models = models(&[], Some(&legacy));
        assert_eq!(models.iter().map(|m| (m.id.as_str(), m.name.as_str(), m.default)).collect::<Vec<_>>(), [("a", "A", false), ("b", "b", true)]);
        assert!(super::models(&[], None).is_empty(), "no model list: nothing to offer");

        let many: Vec<_> = (0..40).map(|i| json!({ "id": format!("o{i}"), "name": "x".repeat(400), "type": "boolean", "currentValue": false })).collect();
        let options = options(&wire::config_options(&json!(many)));
        assert_eq!(options.len(), MAX_OPTIONS);
        let ModelOption::Boolean { label, .. } = &options[0] else { panic!() };
        assert_eq!(label.len(), MAX_TEXT);
        let choices: Vec<_> = (0..100).map(|i| json!({ "value": format!("v{i}"), "name": "é".repeat(200) })).collect();
        let select = wire::config_options(&json!([{ "id": "s", "name": "S", "type": "select", "currentValue": "v0", "options": choices }]));
        let ModelOption::Select { choices, .. } = &super::options(&select)[0] else { panic!() };
        assert_eq!(choices.len(), MAX_CHOICES);
        assert!(choices[0].label.len() <= MAX_TEXT && choices[0].label.chars().all(|c| c == 'é'));
    }
}
