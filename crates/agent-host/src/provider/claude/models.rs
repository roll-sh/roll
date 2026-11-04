//! Claude models and efforts, matching T3 Code's model manifest for its `claudeAgent` driver
//! (`apps/server/src/provider/model-manifest.json`, the "current" models) and how
//! `claudeModelOptions.ts` compiles an effort into CLI flags, and each model's context window
//! (the manifest's `fixedContextWindowTokens` / `contextWindowTokens`, as
//! `resolveClaudeCatalogContextWindowTokens` reads them).

use agent_proto::ModelInfo;

struct Model {
    id: &'static str,
    name: &'static str,
    /// The first Claude Code release that knows the model.
    min_version: (u32, u32, u32),
    efforts: &'static [&'static str],
}

const WITH_ULTRACODE: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultracode", "ultrathink"];
const PLAIN: &[&str] = &["low", "medium", "high", "xhigh", "max", "ultrathink"];

/// T3's default Claude model comes first.
const MODELS: &[Model] = &[
    Model { id: "claude-opus-5-5", name: "Claude Opus 5.5", min_version: (2, 1, 280), efforts: WITH_ULTRACODE },
    Model { id: "claude-sonnet-5-5", name: "Claude Sonnet 5.5", min_version: (2, 1, 284), efforts: PLAIN },
    Model { id: "claude-haiku-5-5", name: "Claude Haiku 5.5", min_version: (2, 1, 293), efforts: PLAIN },
    Model { id: "claude-fable-5-1", name: "Claude Fable 5.1", min_version: (2, 1, 257), efforts: WITH_ULTRACODE },
];

/// The models an installed Claude Code (`version` like `2.1.310`) can run; all of them when the
/// version is unknown.
pub(super) fn models(version: Option<&str>) -> Vec<ModelInfo> {
    let installed = version.and_then(parse_version);
    MODELS
        .iter()
        .filter(|m| installed.is_none_or(|v| v >= m.min_version))
        .enumerate()
        .map(|(i, m)| ModelInfo {
            id: m.id.into(),
            name: m.name.into(),
            default: i == 0,
            efforts: m.efforts.iter().map(|e| e.to_string()).collect(),
        })
        .collect()
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

/// What an effort turns into.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct Effort {
    /// `--effort <level>`.
    pub cli: Option<&'static str>,
    /// `ultracode` in the settings (xhigh effort plus Claude's multi-agent orchestration).
    pub ultracode: bool,
    /// Prompts start with `Ultrathink:` (T3's `applyClaudePromptEffortPrefix`).
    pub ultrathink: bool,
}

pub(super) fn effort(effort: Option<&str>) -> Effort {
    const LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];
    match effort {
        Some(e) if LEVELS.contains(&e) => Effort { cli: LEVELS.into_iter().find(|l| *l == e), ..Effort::default() },
        Some("ultracode") => Effort { cli: Some("xhigh"), ultracode: true, ultrathink: false },
        Some("ultrathink") => Effort { ultrathink: true, ..Effort::default() },
        _ => Effort::default(),
    }
}

/// Prefix a prompt for `ultrathink`, leaving slash commands alone so they still run.
pub(super) fn prompt_text(text: &str, effort: &Effort) -> String {
    let trimmed = text.trim();
    let slash_command = trimmed.strip_prefix('/').is_some_and(|rest| rest.split(char::is_whitespace).next().is_some_and(|name| !name.is_empty() && !name.contains('/')));
    if !effort.ultrathink || trimmed.is_empty() || slash_command || trimmed.starts_with("Ultrathink:") {
        return trimmed.to_string();
    }
    format!("Ultrathink:\n{trimmed}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn models_follow_the_installed_version() {
        let all = models(None);
        assert_eq!(all.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5", "claude-fable-5-1"]);
        assert!(all[0].default && !all[1].default);
        assert_eq!(all[2].efforts.first().map(String::as_str), Some("low"));
        let old = models(Some("2.1.281 (Claude Code)"));
        assert_eq!(old.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["claude-opus-5-5", "claude-fable-5-1"]);
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
    fn efforts_compile_like_t3() {
        assert_eq!(effort(Some("high")).cli, Some("high"));
        assert_eq!(effort(Some("ultracode")), Effort { cli: Some("xhigh"), ultracode: true, ultrathink: false });
        let think = effort(Some("ultrathink"));
        assert_eq!(think.cli, None);
        assert_eq!(prompt_text(" fix it ", &think), "Ultrathink:\nfix it");
        assert_eq!(prompt_text("/compact now", &think), "/compact now");
        assert_eq!(prompt_text("fix it", &effort(None)), "fix it");
    }
}
