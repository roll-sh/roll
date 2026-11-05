//! Pi's models, thinking levels, launch arguments and versions (T3: `status.ts`
//! `parseDiscoveredModels`, `thinkingCapabilities.ts`, and `mcpInjection.ts`
//! `resolvePiLaunchArgs` / `buildPiRpcLaunch`, in `packages/provider-pi/src/server`).

use agent_proto::{ModelInfo, ModelOption, OptionChoice};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;

/// The model that keeps Pi's own configured default (T3's `PI_INHERIT_MODEL_SLUG`).
pub const DEFAULT_MODEL: &str = "default";

/// Pi's thinking levels, lowest first. `xhigh` and `max` need a model's opt-in: offering them
/// to every model makes `set_thinking_level` fail.
pub const LEVELS: [&str; 7] = ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// At most this many models are kept from one listing.
const MAX_MODELS: usize = 512;

/// `get_entries` arrived in 0.80.3 and `agent_settled` in 0.80.4; 0.80.5 was the first release
/// with both (T3's `MINIMUM_PI_VERSION`).
pub const MIN_VERSION: &str = "0.80.5";
/// `pi auth check --json --no-refresh` arrived in 0.84.1.
pub const AUTH_CHECK_VERSION: &str = "0.84.1";

fn level_label(level: &str) -> &'static str {
    match level {
        "off" => "Off",
        "minimal" => "Minimal",
        "low" => "Low",
        "medium" => "Medium",
        "high" => "High",
        "xhigh" => "Extra High",
        _ => "Max",
    }
}

/// The pseudo-model that keeps Pi's default model and thinking level.
pub fn default_model() -> ModelInfo {
    ModelInfo { default: true, ..ModelInfo::new(DEFAULT_MODEL, "Pi default") }
}

/// `provider/model`, split at the first slash (T3's `parsePiModelSlug`).
pub fn parse_slug(slug: &str) -> Option<(&str, &str)> {
    let (provider, model) = slug.split_once('/')?;
    (!provider.is_empty() && !model.is_empty()).then_some((provider, model))
}

fn str_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

/// A model record's `provider/id`.
pub fn slug(model: &Value) -> Option<String> {
    Some(format!("{}/{}", str_at(model, "provider")?, str_at(model, "id")?))
}

/// A model record's context window, when it names one.
pub fn context_window(model: &Value) -> Option<u64> {
    model.get("contextWindow").and_then(Value::as_f64).filter(|w| w.is_finite() && *w >= 1.0).map(|w| w as u64)
}

/// The thinking levels a model takes (T3's `supportedPiThinkingLevelsFromModel`): none unless
/// it reasons; a level its `thinkingLevelMap` maps to `null` is out, and `xhigh` and `max` must
/// be mapped.
pub fn thinking_levels(model: &Value) -> Vec<&'static str> {
    if model.get("reasoning") != Some(&Value::Bool(true)) {
        return Vec::new();
    }
    let map = model.get("thinkingLevelMap").and_then(Value::as_object);
    LEVELS
        .into_iter()
        .filter(|level| match map.and_then(|m| m.get(*level)) {
            Some(Value::Null) => false,
            Some(_) => true,
            None => !matches!(*level, "xhigh" | "max"),
        })
        .collect()
}

/// T3's `clampPiThinkingLevel`: the level itself if offered, else the nearest higher one, else
/// the nearest lower one.
fn clamp(level: &str, offered: &[&'static str]) -> Option<&'static str> {
    let at = LEVELS.iter().position(|l| *l == level)?;
    LEVELS[at..].iter().chain(LEVELS[..at].iter().rev()).find_map(|l| offered.iter().find(|o| *o == l).copied())
}

/// A listed model with its thinking levels; the active model's default is Pi's current level.
fn model_info(model: &Value, active_level: Option<&str>) -> Option<ModelInfo> {
    let slug = slug(model)?;
    let levels = thinking_levels(model);
    let mut info = ModelInfo::new(slug.clone(), str_at(model, "name").unwrap_or(&slug));
    if !levels.is_empty() {
        let default = active_level.and_then(|l| clamp(l, &levels));
        info.options.push(ModelOption::reasoning(levels.iter().map(|l| OptionChoice::new(l, level_label(l))).collect(), default));
    }
    if let Some(input) = model.get("input").and_then(Value::as_array) {
        info.images = input.iter().any(|i| i.as_str() == Some("image"));
    }
    Some(info)
}

/// `get_available_models` as roll's models, given `get_state` for the active model's thinking
/// level (T3's `parseDiscoveredModels`). Pi lists only models whose provider has credentials.
pub fn models(data: &Value, state: &Value) -> Result<Vec<ModelInfo>, String> {
    let list = data.get("models").and_then(Value::as_array).ok_or("pi listed no model inventory")?;
    let active = state.get("model").and_then(slug);
    let level = str_at(state, "thinkingLevel");
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for model in list.iter().take(MAX_MODELS) {
        let slug = slug(model).ok_or("pi listed a model without a provider and id")?;
        if !seen.insert(slug.clone()) {
            continue;
        }
        let level = if active.as_deref() == Some(slug.as_str()) { level } else { None };
        out.extend(model_info(model, level));
    }
    Ok(out)
}

/// Arguments roll sets itself (T3's `RESERVED_PI_LAUNCH_ARGUMENTS`).
const RESERVED: [&str; 17] =
    ["--continue", "-c", "--export", "--fork", "--help", "-h", "--list-models", "--mode", "--no-session", "--print", "-p", "--resume", "-r", "--session", "--session-id", "--version", "-v"];

const WITH_VALUES: [&str; 21] = [
    "--api-key",
    "--append-system-prompt",
    "--exclude-tools",
    "-xt",
    "--extension",
    "-e",
    "--model",
    "--models",
    "--name",
    "-n",
    "--prompt-template",
    "--provider",
    "--session-dir",
    "--skill",
    "--system-prompt",
    "--theme",
    "--thinking",
    "--tools",
    "-t",
    "--tui-mode",
    "--use-theme",
];

const WITHOUT_VALUES: [&str; 19] = [
    "--approve",
    "-a",
    "--no-approve",
    "-na",
    "--no-builtin-tools",
    "-nbt",
    "--no-context-files",
    "-nc",
    "--no-extensions",
    "-ne",
    "--no-prompt-templates",
    "-np",
    "--no-skills",
    "-ns",
    "--no-themes",
    "--no-tools",
    "-nt",
    "--offline",
    "--verbose",
];

/// The machine's launch arguments for Pi, checked (T3's `resolvePiLaunchArgs`): roll's own
/// arguments and positional prompts are refused, `--provider` needs `--model`, and the
/// `--flag=value` form of Pi's own flags is split (Pi reads that form as an extension flag).
pub fn launch_args(args: &[String]) -> Result<Vec<String>, String> {
    let args: Vec<String> = args
        .iter()
        .flat_map(|a| match a.split_once('=') {
            Some((flag, value)) if !flag.is_empty() && WITH_VALUES.contains(&flag) => vec![flag.to_string(), value.to_string()],
            _ => vec![a.clone()],
        })
        .collect();
    let (mut provider, mut model) = (false, false);
    let mut i = 0;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(r) = RESERVED.iter().find(|r| arg == **r || (r.starts_with("--") && arg.starts_with(&format!("{r}=")))) {
            return Err(format!("Pi launch argument '{r}' is controlled by roll and cannot be overridden."));
        }
        if arg == "--" {
            return Err("Pi launch arguments cannot include positional prompts.".into());
        }
        if WITH_VALUES.contains(&arg) {
            if i + 1 >= args.len() {
                return Err(format!("Pi launch argument '{arg}' requires a value."));
            }
            provider |= arg == "--provider";
            model |= arg == "--model";
            i += 2;
            continue;
        }
        if WITHOUT_VALUES.contains(&arg) || (arg.starts_with("--") && arg.contains('=')) {
            i += 1;
            continue;
        }
        if arg.starts_with("--") {
            // An extension's flag may take the next token as its value.
            i += match args.get(i + 1) {
                Some(next) if !next.starts_with('-') && !next.starts_with('@') => 2,
                _ => 1,
            };
            continue;
        }
        if arg.starts_with('-') {
            return Err(format!("Pi launch argument '{arg}' is not supported by roll."));
        }
        return Err(format!("Pi launch arguments cannot include positional prompt '{arg}'."));
    }
    if provider && !model {
        return Err("Pi launch argument '--provider' requires '--model'.".into());
    }
    Ok(args)
}

/// How one Pi process starts (T3's `buildPiRpcLaunch`).
#[derive(Default)]
pub struct Launch<'a> {
    /// The checked launch arguments.
    pub args: &'a [String],
    /// `--no-session`: a probe that leaves no session file behind.
    pub ephemeral: bool,
    /// `--offline`: a process roll stops as soon as it answers. Pi's startup catalog refresh
    /// renews expired OAuth tokens, and stopping it mid-request would lose the rotated one.
    pub short_lived: bool,
    /// `--no-extensions --no-tools`, after the user's arguments so they can't turn them back on
    /// (the process that copies a session for a fork).
    pub bare: bool,
    /// roll's extension (approvals, MCP, instructions).
    pub extension: Option<&'a Path>,
}

pub fn build(l: Launch<'_>) -> Vec<String> {
    let mut user: Vec<String> = l.args.to_vec();
    if l.bare {
        user = without(&user, &["--extension", "-e"], &[], &["--extension=", "-e="]);
        user = without(
            &user,
            &["--tools", "-t", "--exclude-tools", "-xt"],
            &["--no-tools", "-nt", "--no-builtin-tools", "-nbt"],
            &["--tools=", "-t=", "--exclude-tools=", "-xt="],
        );
    }
    let mut args = vec!["--mode".to_string(), "rpc".to_string()];
    if l.ephemeral {
        args.push("--no-session".into());
    }
    let offline = l.short_lived && !user.iter().any(|a| a == "--offline");
    args.extend(user);
    if offline {
        args.push("--offline".into());
    }
    if l.bare {
        args.extend(["--no-extensions".to_string(), "--no-tools".to_string()]);
    }
    if let Some(path) = l.extension.filter(|_| !l.bare) {
        let path = path.display().to_string();
        let given = args.windows(2).any(|w| (w[0] == "--extension" || w[0] == "-e") && w[1].trim_end_matches('/') == path);
        if !given {
            args.extend(["--extension".to_string(), path]);
        }
    }
    args
}

/// `args` without the flags in `with_value` (and their values), the flags in `alone`, and any
/// argument starting with one of `prefixes`.
fn without(args: &[String], with_value: &[&str], alone: &[&str], prefixes: &[&str]) -> Vec<String> {
    let mut out = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if with_value.contains(&a.as_str()) {
            it.next();
        } else if !alone.contains(&a.as_str()) && !prefixes.iter().any(|p| a.starts_with(p)) {
            out.push(a.clone());
        }
    }
    out
}

/// The first `x.y.z` in `pi --version`'s output.
pub fn parse_version(text: &str) -> Option<String> {
    text.split_whitespace()
        .map(|w| w.trim_start_matches('v'))
        .find(|w| w.split('.').count() >= 3 && w.split('.').take(3).all(|p| p.chars().next().is_some_and(|c| c.is_ascii_digit())))
        .map(str::to_string)
}

/// `version >= min`, comparing the numeric `x.y.z` (a pre-release counts as its release).
pub fn at_least(version: &str, min: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.split('.').take(3).map(|p| p.chars().take_while(char::is_ascii_digit).collect::<String>().parse().unwrap_or(0)).collect()
    };
    parts(version) >= parts(min)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn strings(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn thinking_levels_follow_the_models_map() {
        let m = json!({ "reasoning": true, "thinkingLevelMap": { "off": "none", "minimal": null, "low": null, "medium": null, "high": "high", "xhigh": "xhigh", "max": null } });
        assert_eq!(thinking_levels(&m), ["off", "high", "xhigh"]);
        assert_eq!(thinking_levels(&json!({ "reasoning": true })), ["off", "minimal", "low", "medium", "high"]);
        assert!(thinking_levels(&json!({ "reasoning": false })).is_empty());
        assert_eq!(clamp("medium", &["off", "high", "xhigh"]), Some("high"));
        assert_eq!(clamp("max", &["off", "high"]), Some("high"));
        assert_eq!(clamp("nope", &["off"]), None);
    }

    #[test]
    fn models_are_listed_with_the_active_ones_default_level() {
        let data = json!({ "models": [
            { "provider": "openrouter", "id": "deepseek/v4", "name": "DeepSeek", "reasoning": true, "input": ["text"] },
            { "provider": "openrouter", "id": "deepseek/v4" },
            { "provider": "anthropic", "id": "claude", "input": ["text", "image"] },
        ] });
        let state = json!({ "model": { "provider": "openrouter", "id": "deepseek/v4" }, "thinkingLevel": "medium" });
        let models = models(&data, &state).unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "openrouter/deepseek/v4");
        assert!(!models[0].images);
        assert_eq!(models[0].efforts(), ["off", "minimal", "low", "medium", "high"]);
        assert!(matches!(models[0].reasoning(), Some(ModelOption::Select { default: Some(d), .. }) if d == "medium"));
        assert!(models[1].images && models[1].options.is_empty());
        assert!(super::models(&json!({ "models": [{ "id": "x" }] }), &state).is_err());
        assert_eq!(parse_slug("openrouter/deepseek/v4"), Some(("openrouter", "deepseek/v4")));
        assert_eq!(parse_slug("default"), None);
    }

    #[test]
    fn launch_arguments_are_checked() {
        let ok = launch_args(&strings(&["--provider", "openrouter", "--model=x", "--approve", "--my-flag", "value"])).unwrap();
        assert_eq!(ok, strings(&["--provider", "openrouter", "--model", "x", "--approve", "--my-flag", "value"]));
        for (args, why) in [
            (&["--session", "x"][..], "'--session' is controlled by roll"),
            (&["--fork=x"], "'--fork' is controlled by roll"),
            (&["-c"], "'-c' is controlled by roll"),
            (&["hello"], "positional prompt 'hello'"),
            (&["--"], "positional prompts"),
            (&["--provider", "p"], "requires '--model'"),
            (&["--model"], "requires a value"),
            (&["-z"], "'-z' is not supported"),
        ] {
            let err = launch_args(&strings(args)).unwrap_err();
            assert!(err.contains(why), "{args:?}: {err}");
        }
    }

    #[test]
    fn launches_are_built_like_t3s() {
        let user = strings(&["--approve", "-e", "/mine.ts", "--tools", "read", "--offline"]);
        let ext = Path::new("/data/roll-pi-extension.ts");
        assert_eq!(
            build(Launch { args: &user, extension: Some(ext), ..Default::default() }),
            strings(&["--mode", "rpc", "--approve", "-e", "/mine.ts", "--tools", "read", "--offline", "--extension", "/data/roll-pi-extension.ts"])
        );
        assert_eq!(
            build(Launch { args: &user, short_lived: true, bare: true, extension: Some(ext), ..Default::default() }),
            strings(&["--mode", "rpc", "--approve", "--offline", "--no-extensions", "--no-tools"])
        );
        assert_eq!(
            build(Launch { args: &[], ephemeral: true, short_lived: true, ..Default::default() }),
            strings(&["--mode", "rpc", "--no-session", "--offline"])
        );
    }

    #[test]
    fn versions_compare() {
        assert_eq!(parse_version("pi 0.84.2\n").as_deref(), Some("0.84.2"));
        assert_eq!(parse_version("1.0.0").as_deref(), Some("1.0.0"));
        assert_eq!(parse_version("pi"), None);
        assert!(at_least("0.80.5", MIN_VERSION) && at_least("1.0.0", MIN_VERSION) && at_least("0.81.0-beta.1", MIN_VERSION));
        assert!(!at_least("0.80.4", MIN_VERSION) && !at_least("0.9.9", MIN_VERSION));
    }
}
