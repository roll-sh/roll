//! Claude models and their options, ported from T3 Code's model manifest for its `claudeAgent`
//! driver (`apps/server/src/provider/model-manifest.json`: the models, their profiles' option
//! descriptors and adapter data) and how `claudeModelOptions.ts` compiles a selection into the
//! CLI's flags: `--effort` through the profile's `effortMap`, `ultracode`, `fastMode` and
//! `alwaysThinkingEnabled` in `--settings`, a `[1m]` suffix for the 1M context window, and an
//! `Ultrathink:` prompt prefix. Each model's context window follows the manifest's
//! `fixedContextWindowTokens` / `contextWindowTokens`, as `resolveClaudeCatalogContextWindowTokens`
//! reads them.

use agent_proto::{ModelInfo, ModelOption, ModelOptions, OptionChoice, OptionRole, OptionValue, UpdateRequired};

/// A manifest profile: what a family of models takes.
struct Profile {
    efforts: &'static [&'static str],
    default_effort: Option<&'static str>,
    /// Levels the CLI calls by another name (`effortMap`); `ultracode` and `ultrathink` are
    /// handled on their own.
    effort_map: &'static [(&'static str, &'static str)],
    fast: bool,
    /// `contextWindow` 200k/1m, with the default.
    context: Option<&'static str>,
    /// `thinking` on/off (on by default).
    thinking: bool,
}

struct Model {
    id: &'static str,
    name: &'static str,
    /// The first Claude Code release that knows the model.
    min_version: Option<(u32, u32, u32)>,
    legacy: bool,
    profile: &'static Profile,
}

const WITH_ULTRACODE: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"];
const PLAIN: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultrathink"];
const NO_XHIGH: &[&str] = &["low", "medium", "high", "max", "ultrathink"];

const FABLE_5: Profile = Profile { efforts: WITH_ULTRACODE, default_effort: Some("medium"), effort_map: &[], fast: false, context: None, thinking: false };
const OPUS_5_5: Profile = Profile { fast: true, ..FABLE_5 };
const OPUS_5: Profile = Profile { default_effort: Some("high"), ..OPUS_5_5 };
const OPUS_4_7: Profile = Profile { efforts: PLAIN, default_effort: Some("xhigh"), effort_map: &[("xhigh", "max")], fast: true, context: None, thinking: false };
const OPUS_4_6: Profile = Profile { efforts: NO_XHIGH, default_effort: Some("high"), effort_map: &[], fast: true, context: Some("1m"), thinking: false };
const OPUS_4_5: Profile = Profile { efforts: &["low", "medium", "high", "max"], context: None, ..OPUS_4_6 };
const SONNET_5: Profile = Profile { efforts: PLAIN, default_effort: Some("high"), effort_map: &[], fast: false, context: None, thinking: false };
const SONNET_4_6: Profile = Profile { efforts: NO_XHIGH, effort_map: &[("max", "high")], context: Some("200k"), ..SONNET_5 };
const HAIKU_5_5: Profile = Profile { default_effort: Some("medium"), ..SONNET_5 };
const HAIKU_4_5: Profile = Profile { efforts: &[], default_effort: None, effort_map: &[], fast: false, context: None, thinking: true };

const fn v(major: u32, minor: u32, patch: u32) -> Option<(u32, u32, u32)> {
    Some((major, minor, patch))
}

/// T3's default Claude model comes first; legacy ones after the current ones.
const MODELS: &[Model] = &[
    Model { id: "claude-opus-5-5", name: "Claude Opus 5.5", min_version: v(2, 1, 280), legacy: false, profile: &OPUS_5_5 },
    Model { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5", min_version: v(2, 1, 284), legacy: false, profile: &SONNET_5 },
    Model { id: "claude-haiku-5-5", name: "Claude Haiku 5.5", min_version: v(2, 1, 293), legacy: false, profile: &HAIKU_5_5 },
    Model { id: "claude-fable-5-1", name: "Claude Fable 5.1", min_version: v(2, 1, 257), legacy: false, profile: &FABLE_5 },
    Model { id: "claude-fable-5", name: "Claude Fable 5", min_version: v(2, 1, 169), legacy: true, profile: &FABLE_5 },
    Model { id: "claude-opus-5", name: "Claude Opus 5", min_version: v(2, 1, 219), legacy: true, profile: &OPUS_5 },
    Model { id: "claude-opus-4-8", name: "Claude Opus 4.8", min_version: v(2, 1, 154), legacy: true, profile: &OPUS_5 },
    Model { id: "claude-opus-4-7", name: "Claude Opus 4.7", min_version: v(2, 1, 111), legacy: true, profile: &OPUS_4_7 },
    Model { id: "claude-opus-4-6", name: "Claude Opus 4.6", min_version: None, legacy: true, profile: &OPUS_4_6 },
    Model { id: "claude-opus-4-5", name: "Claude Opus 4.5", min_version: None, legacy: true, profile: &OPUS_4_5 },
    Model { id: "claude-sonnet-5", name: "Claude Sonnet 5", min_version: None, legacy: true, profile: &SONNET_5 },
    Model { id: "claude-sonnet-4-6", name: "Claude Sonnet 4.6", min_version: None, legacy: true, profile: &SONNET_4_6 },
    Model { id: "claude-haiku-4-5", name: "Claude Haiku 4.5", min_version: None, legacy: true, profile: &HAIKU_4_5 },
];

pub(super) const FAST_MODE: &str = "fastMode";
pub(super) const CONTEXT_WINDOW: &str = "contextWindow";
pub(super) const THINKING: &str = "thinking";

fn effort_choice(e: &str) -> OptionChoice {
    match e {
        "ultracode" => OptionChoice::new(e, "Ultracode").described("xhigh effort plus multi-agent workflow orchestration"),
        "ultrathink" => OptionChoice::new(e, "Ultrathink").described("Starts each prompt with Ultrathink"),
        other => OptionChoice::new(other, &agent_proto::effort_label(other)),
    }
}

fn options(p: &Profile) -> Vec<ModelOption> {
    let mut out = Vec::new();
    if !p.efforts.is_empty() {
        out.push(ModelOption::reasoning(p.efforts.iter().map(|e| effort_choice(e)).collect(), p.default_effort));
    }
    if p.fast {
        out.push(ModelOption::Boolean {
            id: FAST_MODE.into(),
            label: "Fast Mode".into(),
            description: Some("Faster output at a higher price".into()),
            default: false,
            role: Some(OptionRole::Speed),
        });
    }
    if let Some(default) = p.context {
        out.push(ModelOption::Select {
            id: CONTEXT_WINDOW.into(),
            label: "Context Window".into(),
            description: None,
            choices: vec![OptionChoice::new("200k", "200k"), OptionChoice::new("1m", "1M")],
            default: Some(default.into()),
            role: Some(OptionRole::ContextWindow),
        });
    }
    if p.thinking {
        out.push(ModelOption::Boolean { id: THINKING.into(), label: "Thinking".into(), description: None, default: true, role: None });
    }
    out
}

fn info(m: &Model, default: bool) -> ModelInfo {
    ModelInfo {
        short_name: m.name.strip_prefix("Claude ").map(str::to_string),
        default,
        options: options(m.profile),
        legacy: m.legacy,
        ..ModelInfo::new(m.id, m.name)
    }
}

/// The models an installed Claude Code (`version` like `2.1.310`) can run, all of them when the
/// version is unknown, and the current ones it is too old for.
pub(super) fn models(version: Option<&str>) -> (Vec<ModelInfo>, Vec<UpdateRequired>) {
    let installed = version.and_then(parse_version);
    let runs = |m: &Model| match (installed, m.min_version) {
        (Some(have), Some(need)) => have >= need,
        _ => true,
    };
    let models = MODELS.iter().filter(|m| runs(m)).enumerate().map(|(i, m)| info(m, i == 0)).collect();
    let update = MODELS
        .iter()
        .filter(|m| !m.legacy && !runs(m))
        .filter_map(|m| m.min_version.map(|(a, b, c)| UpdateRequired { id: m.id.into(), name: m.name.into(), min_version: format!("{a}.{b}.{c}") }))
        .collect();
    (models, update)
}

/// The catalog model a slug belongs to: itself, or a dated or `[1m]` form of it
/// (`claude-opus-4-6-20251117`, `claude-sonnet-4-6[1m]`).
fn family(slug: &str) -> Option<&'static Model> {
    let base = slug.strip_suffix("[1m]").unwrap_or(slug);
    MODELS.iter().filter(|m| base == m.id || base.strip_prefix(m.id).is_some_and(|rest| rest.starts_with('-'))).max_by_key(|m| m.id.len())
}

/// The options of a slug the catalog doesn't list (a custom model): its family's.
pub(super) fn options_for(slug: &str) -> Vec<ModelOption> {
    family(slug).map(|m| options(m.profile)).unwrap_or_default()
}

/// `2.1.310 (Claude Code)` → `(2, 1, 310)`.
pub(super) fn parse_version(text: &str) -> Option<(u32, u32, u32)> {
    let word = text.split_whitespace().next()?;
    let mut parts = word.split('.').map(|p| p.parse::<u32>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

/// The context window of a model id as Claude Code names it (`claude-opus-5-5`,
/// `claude-sonnet-4-6[1m]`, `claude-haiku-4-5-20251001`); `None` for one T3's manifest doesn't
/// know (the meter then shows tokens without a percentage).
pub(super) fn context_window(model: &str) -> Option<u64> {
    const M: u64 = 1_000_000;
    const K200: u64 = 200_000;
    let (model, one_m) = match model.strip_suffix("[1m]") {
        Some(base) => (base, true),
        None => (model, false),
    };
    let mut parts = model.strip_prefix("claude-").unwrap_or(model).split('-');
    let family = parts.next()?;
    if !matches!(family, "opus" | "sonnet" | "haiku" | "fable") {
        return None;
    }
    let major: u32 = parts.next()?.parse().ok()?;
    // A date suffix (`-20251001`) is not a minor version.
    let minor: u32 = parts.next().and_then(|p| p.parse().ok()).filter(|m| *m < 100).unwrap_or(0);
    Some(match (family, major, minor) {
        _ if one_m => M,
        // Fable and every 5.x model run with a fixed 1M window.
        (_, 5.., _) | ("fable", ..) => M,
        ("opus", 4, 7..) => M,
        // Opus 4.6 selects 1M by default; Sonnet 4.6 selects 200k.
        ("opus", 4, 6) => M,
        (_, 3 | 4, _) => K200,
        _ => return None,
    })
}

/// What a model, effort and options turn into on the command line.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Compiled {
    /// `--model`: the slug, with `[1m]` for the 1M window of a model that has a choice.
    pub model: Option<String>,
    /// `--effort <level>`.
    pub effort: Option<&'static str>,
    /// `ultracode` in the settings (xhigh effort plus Claude's multi-agent orchestration).
    pub ultracode: bool,
    /// Prompts start with `Ultrathink:` (T3's `applyClaudePromptEffortPrefix`).
    pub ultrathink: bool,
    /// `fastMode` in the settings, for a model with Fast Mode.
    pub fast_mode: Option<bool>,
    /// `alwaysThinkingEnabled` in the settings, for a model with a thinking switch.
    pub thinking: Option<bool>,
}

/// T3's `compileClaudeModelSelection`: an option the model doesn't take is left out; a slug the
/// catalog doesn't know gets its family's treatment, else the plain levels as they are.
pub(super) fn compile(model: Option<&str>, effort: Option<&str>, opts: &ModelOptions) -> Compiled {
    const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    let profile = model.and_then(family).map(|m| m.profile);
    let offered = |e: &str| profile.is_none_or(|p| p.efforts.contains(&e));
    let mut out = Compiled { model: model.map(str::to_string), ..Compiled::default() };
    match effort.filter(|e| offered(e)) {
        Some("ultracode") => (out.effort, out.ultracode) = (Some("xhigh"), true),
        Some("ultrathink") => out.ultrathink = true,
        Some(e) => {
            let mapped = profile.and_then(|p| p.effort_map.iter().find(|(from, _)| *from == e)).map_or(e, |(_, to)| *to);
            out.effort = LEVELS.into_iter().find(|l| *l == mapped);
        }
        None => {}
    }
    let flag = |id: &str| match opts.get(id) {
        Some(OptionValue::Flag(b)) => Some(*b),
        _ => None,
    };
    if let Some(p) = profile {
        if p.fast {
            out.fast_mode = Some(flag(FAST_MODE).unwrap_or(false));
        }
        if p.thinking {
            out.thinking = flag(THINKING);
        }
        if p.context.is_some()
            && let (Some(m), Some(OptionValue::Choice(c))) = (&mut out.model, opts.get(CONTEXT_WINDOW))
        {
            let base = m.strip_suffix("[1m]").unwrap_or(m).to_string();
            *m = if c == "1m" { format!("{base}[1m]") } else { base };
        }
    }
    out
}

/// Prefix a prompt for `ultrathink`, leaving slash commands alone so they still run.
pub(super) fn prompt_text(text: &str, compiled: &Compiled) -> String {
    let trimmed = text.trim();
    let slash_command = trimmed.strip_prefix('/').is_some_and(|rest| rest.split(char::is_whitespace).next().is_some_and(|name| !name.is_empty() && !name.contains('/')));
    if !compiled.ultrathink || trimmed.is_empty() || slash_command || trimmed.starts_with("Ultrathink:") {
        return trimmed.to_string();
    }
    format!("Ultrathink:\n{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().filter(|m| !m.legacy).map(|m| m.id.as_str()).collect()
    }

    #[test]
    fn models_follow_the_installed_version() {
        let (all, update) = models(None);
        assert_eq!(ids(&all), ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"]);
        assert!(update.is_empty());
        assert!(all[0].default && !all[1].default);
        assert_eq!(all[0].short_name.as_deref(), Some("Opus 5.5"));
        assert_eq!(all[2].efforts().first(), Some(&"low"));
        // Legacy models follow the current ones.
        let first_legacy = all.iter().position(|m| m.legacy).unwrap();
        assert!(all[first_legacy..].iter().all(|m| m.legacy));
        assert!(all.iter().any(|m| m.id == "claude-sonnet-4-6" && m.legacy));
        let (old, update) = models(Some("2.1.281 (Claude Code)"));
        assert_eq!(ids(&old), ["claude-opus-5-5", "claude-fable-5-1"]);
        assert_eq!(update.iter().map(|u| (u.id.as_str(), u.min_version.as_str())).collect::<Vec<_>>(), [("claude-sonnet-5-5", "2.1.284"), ("claude-haiku-5-5", "2.1.293")]);
        // Legacy models too old for the CLI are left out, not offered as an update.
        let (older, update) = models(Some("2.1.100"));
        assert!(!older.iter().any(|m| m.id == "claude-opus-4-7"));
        assert!(older.iter().any(|m| m.id == "claude-opus-4-6"));
        assert!(!update.iter().any(|u| u.id == "claude-opus-4-7"));
    }

    #[test]
    fn options_follow_t3s_profiles() {
        let (all, _) = models(None);
        let get = |id: &str| all.iter().find(|m| m.id == id).unwrap();
        let reasoning_default = |m: &ModelInfo| match m.reasoning() {
            Some(ModelOption::Select { default, .. }) => default.clone(),
            _ => None,
        };
        let opus = get("claude-opus-5-5");
        assert_eq!(opus.efforts(), ["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"]);
        assert_eq!(reasoning_default(opus).as_deref(), Some("medium"));
        assert_eq!(opus.option(FAST_MODE).and_then(ModelOption::role), Some(OptionRole::Speed));
        assert!(get("claude-fable-5-1").option(FAST_MODE).is_none());
        assert_eq!(reasoning_default(get("claude-sonnet-5-5")).as_deref(), Some("high"));
        assert_eq!(reasoning_default(get("claude-opus-4-7")).as_deref(), Some("xhigh"));
        let Some(ModelOption::Select { default, role, .. }) = get("claude-sonnet-4-6").option(CONTEXT_WINDOW) else { panic!() };
        assert_eq!((default.as_deref(), *role), (Some("200k"), Some(OptionRole::ContextWindow)));
        let haiku = get("claude-haiku-4-5");
        assert!(haiku.efforts().is_empty());
        assert!(matches!(haiku.option(THINKING), Some(ModelOption::Boolean { default: true, .. })));
        // A custom model of a known family gets its options.
        assert_eq!(options_for("claude-opus-4-6-20251117"), get("claude-opus-4-6").options);
        assert_eq!(options_for("claude-opus-5-5[1m]"), opus.options);
        assert!(options_for("claude-opus-9").is_empty());
    }

    #[test]
    fn context_windows_follow_t3s_manifest() {
        assert_eq!(context_window("claude-opus-5-5"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-5-5"), Some(1_000_000));
        assert_eq!(context_window("claude-fable-5-1"), Some(1_000_000));
        assert_eq!(context_window("claude-opus-4-6"), Some(1_000_000));
        assert_eq!(context_window("claude-sonnet-4-6"), Some(200_000));
        assert_eq!(context_window("claude-sonnet-4-6[1m]"), Some(1_000_000));
        assert_eq!(context_window("claude-haiku-4-5-20251001"), Some(200_000));
        assert_eq!(context_window("claude-opus-4-5"), Some(200_000));
        assert_eq!(context_window("<synthetic>"), None);
        assert_eq!(context_window("gpt-5"), None);
        assert_eq!(context_window("opus"), None);
    }

    #[test]
    fn selections_compile_like_t3() {
        let none = ModelOptions::new();
        let opus = Some("claude-opus-5-5");
        assert_eq!(compile(opus, Some("high"), &none).effort, Some("high"));
        let uc = compile(opus, Some("ultracode"), &none);
        assert_eq!((uc.effort, uc.ultracode, uc.ultrathink), (Some("xhigh"), true, false));
        let think = compile(opus, Some("ultrathink"), &none);
        assert_eq!(think.effort, None);
        assert_eq!(prompt_text(" fix it ", &think), "Ultrathink:\nfix it");
        assert_eq!(prompt_text("/compact now", &think), "/compact now");
        assert_eq!(prompt_text("fix it", &compile(None, None, &none)), "fix it");
        // effortMap: Opus 4.7's xhigh is the CLI's max, Sonnet 4.6's max its high.
        assert_eq!(compile(Some("claude-opus-4-7"), Some("xhigh"), &none).effort, Some("max"));
        assert_eq!(compile(Some("claude-sonnet-4-6"), Some("max"), &none).effort, Some("high"));
        // A level the model doesn't offer is left out; an unknown model passes plain levels on.
        assert_eq!(compile(Some("claude-opus-4-5"), Some("xhigh"), &none).effort, None);
        assert_eq!(compile(Some("my-proxy-model"), Some("max"), &none).effort, Some("max"));
        assert_eq!(compile(Some("my-proxy-model"), Some("ultra"), &none).effort, None);
        // Fast Mode is always said for a model that has it, and only for one.
        let fast = ModelOptions::from([(FAST_MODE.into(), OptionValue::Flag(true))]);
        assert_eq!(compile(opus, None, &fast).fast_mode, Some(true));
        assert_eq!(compile(opus, None, &none).fast_mode, Some(false));
        assert_eq!(compile(Some("claude-fable-5-1"), None, &fast).fast_mode, None);
        // The 1M window is a model suffix, only for a model with the choice.
        let one_m = ModelOptions::from([(CONTEXT_WINDOW.into(), OptionValue::Choice("1m".into()))]);
        assert_eq!(compile(Some("claude-sonnet-4-6"), None, &one_m).model.as_deref(), Some("claude-sonnet-4-6[1m]"));
        let k200 = ModelOptions::from([(CONTEXT_WINDOW.into(), OptionValue::Choice("200k".into()))]);
        assert_eq!(compile(Some("claude-opus-4-6[1m]"), None, &k200).model.as_deref(), Some("claude-opus-4-6"));
        assert_eq!(compile(opus, None, &one_m).model.as_deref(), Some("claude-opus-5-5"));
        // Haiku 4.5's thinking switch.
        let off = ModelOptions::from([(THINKING.into(), OptionValue::Flag(false))]);
        assert_eq!(compile(Some("claude-haiku-4-5"), None, &off).thinking, Some(false));
        assert_eq!(compile(Some("claude-haiku-4-5"), None, &none).thinking, None);
    }
}
