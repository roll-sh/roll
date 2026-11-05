//! What roll answers on the user's behalf, and how its modes map onto an agent's settings.
//!
//! Ported from T3 Code (MIT): `packages/provider-acp/src/server/clientPolicy.ts` (reads never
//! ask; full access allows; auto-edit allows edits whose every path is inside the workspace, by
//! the real path of the nearest existing ancestor, failing closed; a command approval lets the
//! agent run client terminals for the turn or the session), `adapter.ts` (plan mode is the
//! agent's mode named `plan` or `architect`) and the Kiro and fx flavors (`autopilot`, and fx's
//! `mode` of `ask`/`auto`).

use super::wire::{ConfigOption, ConfigValue, Modes};
use agent_proto::{InteractionMode, RuntimeMode};
use std::path::{Component, Path, PathBuf};

/// The ACP tool kinds policy tells apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `read`, `search`, `think`: never ask.
    Read,
    /// `edit`, `delete`, `move`.
    Edit,
    Execute,
    Other,
}

impl Kind {
    pub fn of(kind: &str) -> Kind {
        match kind {
            "read" | "search" | "think" => Kind::Read,
            "edit" | "delete" | "move" => Kind::Edit,
            "execute" => Kind::Execute,
            _ => Kind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Disposition {
    Allow,
    Ask,
}

/// Whether to answer a permission request without the user.
pub fn disposition(runtime: RuntimeMode, kind: Kind, locations: &[String], cwd: &Path) -> Disposition {
    match (runtime, kind) {
        (_, Kind::Read) | (RuntimeMode::FullAccess, _) => Disposition::Allow,
        (RuntimeMode::AutoEdit, Kind::Edit) if !locations.is_empty() && locations.iter().all(|l| inside(cwd, l)) => {
            Disposition::Allow
        }
        _ => Disposition::Ask,
    }
}

/// Approvals that let the agent use the client's own methods: ACP can't tie a terminal or a
/// file write to the permission that allowed it, so an approved command allows terminals and
/// an approved edit allows writes, for the turn or (accepted for the session) the session.
#[derive(Clone, Debug, Default)]
pub struct Grants {
    execute_session: bool,
    execute_turn: Option<u32>,
    edit_session: bool,
    edit_turn: Option<u32>,
}

impl Grants {
    pub fn record(&mut self, kind: Kind, session: bool, turn: Option<u32>) {
        match (kind, session) {
            (Kind::Execute, true) => self.execute_session = true,
            (Kind::Execute, false) => self.execute_turn = turn,
            (Kind::Edit, true) => self.edit_session = true,
            (Kind::Edit, false) => self.edit_turn = turn,
            _ => {}
        }
    }
}

/// What the agent may do through `terminal/*` and `fs/write_text_file` (always inside the
/// workspace; reads are always allowed there).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Access {
    pub execute: bool,
    pub write: bool,
}

pub fn access(runtime: RuntimeMode, grants: &Grants, turn: Option<u32>) -> Access {
    let this_turn = |t: Option<u32>| t.is_some() && t == turn;
    let execute = runtime == RuntimeMode::FullAccess || grants.execute_session || this_turn(grants.execute_turn);
    let write = runtime != RuntimeMode::Ask || grants.edit_session || this_turn(grants.edit_turn);
    Access { execute, write }
}

/// `path` (absolute, or relative to `root`) is inside `root`, following symlinks of whatever
/// exists of it; anything unresolvable is outside.
pub fn inside(root: &Path, path: &str) -> bool {
    contained(root, path).is_some()
}

/// The real path `path` names inside `root`: symlinks resolved for the part that exists (so a
/// link can't lead out), lexical for the rest. `None` when it leaves `root` or can't be read.
pub fn contained(root: &Path, path: &str) -> Option<PathBuf> {
    let root = std::fs::canonicalize(root).ok()?;
    let joined = root.join(path);
    let mut lexical = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::ParentDir => {
                if !lexical.pop() {
                    return None;
                }
            }
            Component::CurDir => {}
            other => lexical.push(other),
        }
    }
    // realpath of the nearest existing ancestor, then the missing rest (T3:
    // `acpCanonicalPathForContainment`).
    let mut existing = lexical.as_path();
    let mut rest = Vec::new();
    let real = loop {
        match std::fs::canonicalize(existing) {
            Ok(real) => break real,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // A dangling symlink is something that exists: refuse it.
                if std::fs::symlink_metadata(existing).is_ok() {
                    return None;
                }
                rest.push(existing.file_name()?.to_os_string());
                existing = existing.parent()?;
            }
            Err(_) => return None,
        }
    };
    let full = rest.iter().rev().fold(real, |p, part| p.join(part));
    full.starts_with(&root).then_some(full)
}

/// The option that picks the model: `model`, else the first of category `model`.
pub fn model_option(config: &[ConfigOption]) -> Option<&ConfigOption> {
    let selects = || config.iter().filter(|o| matches!(o.value, ConfigValue::Select { .. }));
    selects().find(|o| o.id == "model").or_else(|| selects().find(|o| o.category.as_deref() == Some("model")))
}

pub fn mode_option(config: &[ConfigOption]) -> Option<&ConfigOption> {
    config.iter().find(|o| o.category.as_deref() == Some("mode") && matches!(o.value, ConfigValue::Select { .. }))
}

fn effort_option(config: &[ConfigOption]) -> Option<&ConfigOption> {
    config.iter().find(|o| {
        matches!(o.value, ConfigValue::Select { .. })
            && (matches!(o.id.as_str(), "effort" | "effortLevel" | "reasoning_effort" | "thought_level")
                || o.category.as_deref() == Some("thought_level"))
    })
}

/// Copilot names modes by URL (`…/session-modes#plan`); the name after `#` counts.
pub fn is_plan_mode(id: &str) -> bool {
    let name = id.rsplit('#').next().unwrap_or(id);
    name == "plan" || name == "architect"
}

/// A change to send before the next prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Setting {
    /// `session/set_config_option`. An `optional` one may fail without failing the session.
    Option { id: String, value: OptionValue, optional: bool },
    /// `session/set_mode` (v1 agents without a mode option).
    Mode(String),
    /// `session/set_model` (v1's unstable session models).
    Model(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OptionValue {
    Id(String),
    Bool(bool),
}

pub struct Wanted<'a> {
    pub model: Option<&'a str>,
    pub effort: Option<&'a str>,
    /// The thread's other options: each goes to the agent's config option of the same id,
    /// when that option takes the value (T3 hands the agent's own option ids to the picker).
    pub options: &'a agent_proto::ModelOptions,
    pub runtime: RuntimeMode,
    pub interaction: InteractionMode,
    /// The session just opened: fx takes its permission mode only once written (T3 forces
    /// that first write).
    pub opening: bool,
}

/// The settings that bring the agent to what roll wants, in T3's order: model, effort, then
/// the permission mode and plan mode. Only what differs from the agent's current values.
pub fn settings(
    config: &[ConfigOption],
    modes: Option<&Modes>,
    models_current: Option<&str>,
    has_models: bool,
    before_plan: Option<&str>,
    want: &Wanted<'_>,
) -> Vec<Setting> {
    let mut out = Vec::new();
    let id = |value: &str| OptionValue::Id(value.to_string());
    let set = |out: &mut Vec<Setting>, option: &ConfigOption, value: &str| {
        if option.current() != Some(value) {
            out.push(Setting::Option { id: option.id.clone(), value: id(value), optional: false });
        }
    };
    if let Some(model) = want.model {
        match model_option(config) {
            // Written even when unlisted: Kiro lists a model only after it is chosen.
            Some(option) => set(&mut out, option, model),
            None if has_models => {
                if models_current != Some(model) {
                    out.push(Setting::Model(model.to_string()))
                }
            }
            // Kiro 2.32 advertises no model option until one is written (T3 writes `model`
            // anyway); an agent without one just says no.
            None if !config.is_empty() => out.push(Setting::Option { id: "model".into(), value: id(model), optional: true }),
            None => {}
        }
    }
    if let Some(effort) = want.effort
        && let Some(option) = effort_option(config)
        && (option.offers(effort) || option.values().is_empty())
    {
        set(&mut out, option, effort);
    }
    // Modes and the model have their own controls; the reasoning option is `effort`.
    let reserved = |o: &ConfigOption| matches!(o.category.as_deref(), Some("model" | "mode" | "thought_level" | "collaboration_mode")) || o.id == "autopilot";
    for (want_id, value) in want.options {
        let Some(option) = config.iter().find(|o| &o.id == want_id && !reserved(o)) else { continue };
        match (&option.value, value) {
            (ConfigValue::Select { .. }, agent_proto::OptionValue::Choice(v)) if option.offers(v) => set(&mut out, option, v),
            (ConfigValue::Boolean(current), agent_proto::OptionValue::Flag(b)) if current != b => {
                out.push(Setting::Option { id: option.id.clone(), value: OptionValue::Bool(*b), optional: false })
            }
            _ => {}
        }
    }
    // Kiro: Autopilot runs everything without asking; only full access may.
    let full = want.runtime == RuntimeMode::FullAccess;
    if let Some(option) = config.iter().find(|o| o.id == "autopilot") {
        match &option.value {
            ConfigValue::Boolean(current) if *current != full => {
                out.push(Setting::Option { id: option.id.clone(), value: OptionValue::Bool(full), optional: false })
            }
            ConfigValue::Select { .. } => set(&mut out, option, if full { "on" } else { "off" }),
            ConfigValue::Boolean(_) => {}
        }
    }
    // The mode: plan when asked for and offered; fx's `ask`/`auto` for the runtime mode; else
    // back from plan to where the session was.
    let (mode_ids, current): (Vec<&str>, Option<&str>) = match (mode_option(config), modes) {
        (Some(option), _) => (option.values().iter().map(|c| c.id.as_str()).collect(), option.current()),
        (None, Some(modes)) => (modes.available.iter().map(|c| c.id.as_str()).collect(), Some(modes.current.as_str())),
        (None, None) => (Vec::new(), None),
    };
    let plan = mode_ids.iter().copied().find(|id| is_plan_mode(id));
    // fx: `ask`, and `auto` (`code` before fx 0.0.14) that runs without asking.
    let fx_auto = ["auto", "code"].into_iter().find(|m| mode_ids.contains(m));
    let fx = mode_ids.contains(&"ask") && fx_auto.is_some();
    let target = match (want.interaction, plan) {
        (InteractionMode::Plan, Some(plan)) => Some(plan),
        _ if fx => Some(if full { fx_auto.unwrap_or("auto") } else { "ask" }),
        _ if current.is_some_and(is_plan_mode) => before_plan
            .filter(|m| mode_ids.contains(m) && !is_plan_mode(m))
            .or_else(|| mode_ids.iter().copied().find(|id| !is_plan_mode(id))),
        _ => None,
    };
    if let Some(target) = target
        && (current != Some(target) || (fx && want.opening))
    {
        match mode_option(config) {
            Some(option) => out.push(Setting::Option { id: option.id.clone(), value: id(target), optional: false }),
            None => out.push(Setting::Mode(target.to_string())),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::acp::wire;
    use serde_json::json;

    #[test]
    fn edits_are_allowed_only_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        std::os::unix::fs::symlink("/etc", root.join("etc")).unwrap();
        std::os::unix::fs::symlink(root.join("missing"), root.join("dangling")).unwrap();
        let abs = |p: &str| root.join(p).display().to_string();
        assert!(inside(root, &abs("src/new/file.rs")));
        assert!(inside(root, "src/lib.rs"));
        assert!(!inside(root, &abs("../x")));
        assert!(!inside(root, "/etc/passwd"));
        assert!(!inside(root, &abs("etc/passwd")));
        assert!(!inside(root, &abs("dangling/x")));
        assert!(!inside(root, &abs("src/../../x")));

        let ask = disposition(RuntimeMode::AutoEdit, Kind::Edit, &[abs("etc/hosts")], root);
        assert_eq!(ask, Disposition::Ask);
        assert_eq!(disposition(RuntimeMode::AutoEdit, Kind::Edit, &[abs("src/a")], root), Disposition::Allow);
        assert_eq!(disposition(RuntimeMode::AutoEdit, Kind::Edit, &[], root), Disposition::Ask);
        assert_eq!(disposition(RuntimeMode::AutoEdit, Kind::Execute, &[], root), Disposition::Ask);
        assert_eq!(disposition(RuntimeMode::Ask, Kind::Read, &[], root), Disposition::Allow);
        assert_eq!(disposition(RuntimeMode::FullAccess, Kind::Other, &[], root), Disposition::Allow);
    }

    #[test]
    fn grants_last_for_the_turn_or_the_session() {
        let mut grants = Grants::default();
        assert_eq!(access(RuntimeMode::Ask, &grants, Some(1)), Access { execute: false, write: false });
        assert_eq!(access(RuntimeMode::AutoEdit, &grants, Some(1)), Access { execute: false, write: true });
        grants.record(Kind::Execute, false, Some(1));
        assert!(access(RuntimeMode::Ask, &grants, Some(1)).execute);
        assert!(!access(RuntimeMode::Ask, &grants, Some(2)).execute);
        grants.record(Kind::Edit, true, Some(2));
        assert!(access(RuntimeMode::Ask, &grants, Some(9)).write);
    }

    fn want(runtime: RuntimeMode, interaction: InteractionMode) -> Wanted<'static> {
        static NONE: std::sync::LazyLock<agent_proto::ModelOptions> = std::sync::LazyLock::new(Default::default);
        Wanted { model: None, effort: None, options: &NONE, runtime, interaction, opening: false }
    }

    #[test]
    fn options_go_to_the_agents_own_config_options() {
        let agent = wire::config_options(&json!([
            { "id": "verbosity", "type": "select", "currentValue": "low", "options": [{ "value": "low" }, { "value": "high" }] },
            { "id": "web_search", "type": "boolean", "currentValue": false },
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "ask", "options": [{ "value": "ask" }, { "value": "yolo" }] },
        ]));
        let options = agent_proto::ModelOptions::from([
            ("verbosity".into(), agent_proto::OptionValue::Choice("high".into())),
            ("web_search".into(), agent_proto::OptionValue::Flag(true)),
            // Not offered, not the agent's, and a mode (roll's runtime mode decides it): skipped.
            ("unknown".into(), agent_proto::OptionValue::Flag(true)),
            ("mode".into(), agent_proto::OptionValue::Choice("yolo".into())),
        ]);
        let s = settings(&agent, None, None, false, None, &Wanted { options: &options, ..want(RuntimeMode::Ask, InteractionMode::Default) });
        assert_eq!(s, [
            Setting::Option { id: "verbosity".into(), value: OptionValue::Id("high".into()), optional: false },
            Setting::Option { id: "web_search".into(), value: OptionValue::Bool(true), optional: false },
        ]);
        let wrong = agent_proto::ModelOptions::from([("verbosity".into(), agent_proto::OptionValue::Choice("max".into()))]);
        assert!(settings(&agent, None, None, false, None, &Wanted { options: &wrong, ..want(RuntimeMode::Ask, InteractionMode::Default) }).is_empty());
    }

    #[test]
    fn modes_map_onto_kiro_fx_and_legacy_agents() {
        // Kiro: a mode option with `plan`, and Autopilot.
        let kiro = wire::config_options(&json!([
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "vibe", "options": [{ "value": "vibe" }, { "value": "plan" }] },
            { "id": "autopilot", "type": "select", "currentValue": "on", "options": [{ "value": "on" }, { "value": "off" }] },
            { "id": "model", "category": "model", "type": "select", "currentValue": "auto", "options": [{ "value": "auto" }] },
        ]));
        let s = settings(&kiro, None, None, false, None, &Wanted { model: Some("claude-haiku-4.5"), ..want(RuntimeMode::Ask, InteractionMode::Plan) });
        assert_eq!(s, [
            Setting::Option { id: "model".into(), value: OptionValue::Id("claude-haiku-4.5".into()), optional: false },
            Setting::Option { id: "autopilot".into(), value: OptionValue::Id("off".into()), optional: false },
            Setting::Option { id: "mode".into(), value: OptionValue::Id("plan".into()), optional: false },
        ]);
        assert!(settings(&kiro, None, None, false, None, &want(RuntimeMode::FullAccess, InteractionMode::Default)).is_empty());

        // fx: `mode` is `ask` or `auto`; effort is offered.
        let fx = wire::config_options(&json!([
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "auto", "options": [{ "value": "ask" }, { "value": "auto" }] },
            { "id": "effort", "type": "select", "currentValue": "high", "options": [{ "value": "medium" }, { "value": "high" }] },
        ]));
        let s = settings(&fx, None, None, false, None, &Wanted { effort: Some("medium"), ..want(RuntimeMode::Ask, InteractionMode::Default) });
        assert_eq!(s, [
            Setting::Option { id: "effort".into(), value: OptionValue::Id("medium".into()), optional: false },
            Setting::Option { id: "mode".into(), value: OptionValue::Id("ask".into()), optional: false },
        ]);
        // Older fx says `code`; a fresh session writes the mode even when it is current.
        let old_fx = wire::config_options(&json!([
            { "id": "mode", "category": "mode", "type": "select", "currentValue": "ask", "options": [{ "value": "code" }, { "value": "ask" }] },
        ]));
        assert!(settings(&old_fx, None, None, false, None, &want(RuntimeMode::Ask, InteractionMode::Default)).is_empty());
        let s = settings(&old_fx, None, None, false, None, &Wanted { opening: true, ..want(RuntimeMode::FullAccess, InteractionMode::Default) });
        assert_eq!(s, [Setting::Option { id: "mode".into(), value: OptionValue::Id("code".into()), optional: false }]);
        // No model option yet (Kiro): written anyway, allowed to fail.
        let s = settings(&old_fx, None, None, false, None, &Wanted { model: Some("m"), ..want(RuntimeMode::Ask, InteractionMode::Default) });
        assert_eq!(s, [Setting::Option { id: "model".into(), value: OptionValue::Id("m".into()), optional: true }]);

        // Legacy modes, a Copilot-style plan id, and leaving plan for the mode before it.
        let modes = wire::Modes {
            current: "https://agentclientprotocol.com/protocol/session-modes#plan".into(),
            available: ["default", "yolo", "https://agentclientprotocol.com/protocol/session-modes#plan"]
                .map(|id| wire::Choice { id: id.into(), name: id.into() })
                .to_vec(),
        };
        let s = settings(&[], Some(&modes), Some("m"), true, Some("yolo"), &Wanted { model: Some("n"), ..want(RuntimeMode::FullAccess, InteractionMode::Default) });
        assert_eq!(s, [Setting::Model("n".into()), Setting::Mode("yolo".into())]);
        assert!(is_plan_mode("architect") && !is_plan_mode("planner"));
    }
}
