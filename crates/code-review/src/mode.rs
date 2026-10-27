//! How deep a review goes, and — with `mode: auto` — which depth a change needs.
//!
//! The heuristic scores a change from facts code-control gathers ([`RiskInput`]): its size,
//! the paths it touches (configured `deep_paths` / `max_paths`, and sensitive areas like auth,
//! crypto, migrations and concurrency), how many callers its changed symbols have, whether
//! tests moved with the code, the author's recent reverts and queue ejections, and the linked
//! work item's labels. Every rule that fired becomes a reason, shown at the top of the review.

use crate::glob;
use crate::settings::AutoSettings;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// CodeRabbit-style: walkthrough, per-file summaries, inline comments, incremental.
    Standard,
    /// Greptile-style: standard plus repository context and confidence scores.
    Deep,
    /// Deep plus a verification pass that drops findings it can't confirm.
    Max,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Standard => "standard",
            Mode::Deep => "deep",
            Mode::Max => "max",
        }
    }
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "standard" => Mode::Standard,
            "deep" => Mode::Deep,
            "max" => Mode::Max,
            _ => return None,
        })
    }
}

/// `auto` or a fixed mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModeChoice {
    Auto,
    Fixed(Mode),
}

impl ModeChoice {
    pub fn parse(s: &str) -> Option<ModeChoice> {
        if s == "auto" { Some(ModeChoice::Auto) } else { Mode::parse(s).map(ModeChoice::Fixed) }
    }
    pub fn as_str(&self) -> &'static str {
        match self {
            ModeChoice::Auto => "auto",
            ModeChoice::Fixed(m) => m.as_str(),
        }
    }
}

impl Serialize for ModeChoice {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for ModeChoice {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        ModeChoice::parse(&s).ok_or_else(|| serde::de::Error::custom("auto, standard, deep or max"))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Speed {
    /// The secondary model at low effort, file groups in parallel, comments as each finishes,
    /// no diagram or verification.
    Fast,
    Balanced,
    /// Higher effort, verification and wider retrieval.
    Thorough,
}

impl Speed {
    pub fn as_str(&self) -> &'static str {
        match self {
            Speed::Fast => "fast",
            Speed::Balanced => "balanced",
            Speed::Thorough => "thorough",
        }
    }
    pub fn parse(s: &str) -> Option<Speed> {
        Some(match s {
            "fast" => Speed::Fast,
            "balanced" => Speed::Balanced,
            "thorough" => Speed::Thorough,
            _ => return None,
        })
    }
}

/// What the `auto` heuristic looks at.
#[derive(Clone, Debug, Default)]
pub struct RiskInput {
    /// Lines added + removed.
    pub lines: u64,
    pub paths: Vec<String>,
    /// Paths with a CODEOWNERS entry.
    pub codeowned: usize,
    /// The most callers any changed symbol has (from the codebase index).
    pub fanout: u64,
    pub tests_changed: bool,
    /// The author's changes reverted, and ejected from the merge queue, in the last 30 days.
    pub author_reverts: u32,
    pub author_ejections: u32,
    /// The linked work item's labels.
    pub labels: Vec<String>,
}

/// Path fragments that make a change sensitive: `deep`, or `max` for the second list.
const SENSITIVE: [&str; 14] = [
    "auth", "login", "session", "permission", "acl", "security", "migration", "payment", "billing", "credential", "secret", "mutex", "concurren", "unsafe",
];
const CRITICAL: [&str; 4] = ["crypto", "cipher", "keystore", "signing"];

/// Pick a mode for a change, with the reasons.
pub fn score(input: &RiskInput, auto: &AutoSettings) -> (Mode, Vec<String>) {
    let mut mode = Mode::Standard;
    let mut reasons = Vec::new();
    let mut up = |m: Mode, why: String, mode: &mut Mode| {
        if m > *mode {
            *mode = m;
        }
        reasons.push(why);
    };
    let modules: std::collections::BTreeSet<&str> = input.paths.iter().map(|p| p.split('/').next().unwrap_or(p)).collect();
    if let Some(p) = input.paths.iter().find(|p| glob::any(&auto.max_paths, p)) {
        up(Mode::Max, format!("{p} matches max_paths"), &mut mode);
    }
    if let Some(p) = input.paths.iter().find(|p| glob::any(&auto.deep_paths, p)) {
        up(Mode::Deep, format!("{p} matches deep_paths"), &mut mode);
    }
    let lower: Vec<String> = input.paths.iter().map(|p| p.to_ascii_lowercase()).collect();
    if let Some((p, w)) = lower.iter().find_map(|p| CRITICAL.iter().find(|w| p.contains(**w)).map(|w| (p, w))) {
        up(Mode::Max, format!("touches {w} code ({p})"), &mut mode);
    } else if let Some((p, w)) = lower.iter().find_map(|p| SENSITIVE.iter().find(|w| p.contains(**w)).map(|w| (p, w))) {
        up(Mode::Deep, format!("touches sensitive code: {w} ({p})"), &mut mode);
    }
    for l in &input.labels {
        let l = l.to_ascii_lowercase();
        if l == "security" || l == "critical" || l == "p0" {
            up(Mode::Max, format!("labelled {l}"), &mut mode);
        } else if l == "risky" || l == "p1" || l == "breaking" {
            up(Mode::Deep, format!("labelled {l}"), &mut mode);
        }
    }
    if input.fanout >= 20 {
        up(Mode::Deep, format!("a changed symbol has {} callers", input.fanout), &mut mode);
    }
    if input.lines >= 1500 {
        up(Mode::Deep, format!("{} lines changed", input.lines), &mut mode);
    }
    if modules.len() >= 5 {
        up(Mode::Deep, format!("touches {} top-level modules", modules.len()), &mut mode);
    }
    if input.lines >= 200 && !input.tests_changed && input.paths.iter().any(|p| !is_doc(p)) {
        up(Mode::Deep, format!("{} lines of code changed without tests", input.lines), &mut mode);
    }
    if input.codeowned > 0 && input.lines >= auto.standard_below_lines {
        up(Mode::Deep, format!("{} code-owned files", input.codeowned), &mut mode);
    }
    let history = input.author_reverts + input.author_ejections;
    if history >= 2 {
        up(Mode::Deep, format!("the author had {history} reverts or queue ejections in 30 days"), &mut mode);
    }
    if reasons.is_empty() {
        let why = if input.lines < auto.standard_below_lines {
            format!("small change ({} lines), nothing sensitive", input.lines)
        } else {
            format!("{} lines, nothing sensitive", input.lines)
        };
        reasons.push(why);
    }
    (mode, reasons)
}

fn is_doc(p: &str) -> bool {
    let l = p.to_ascii_lowercase();
    l.ends_with(".md") || l.ends_with(".txt") || l.ends_with(".rst") || l.starts_with("docs/")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(lines: u64, paths: &[&str]) -> RiskInput {
        RiskInput { lines, paths: paths.iter().map(|p| p.to_string()).collect(), tests_changed: true, ..Default::default() }
    }

    #[test]
    fn routing() {
        let auto = AutoSettings { deep_paths: vec!["billing/**".into()], max_paths: vec!["**/kms/**".into()], ..Default::default() };
        let (m, r) = score(&input(10, &["src/a.rs"]), &auto);
        assert_eq!(m, Mode::Standard);
        assert!(r[0].contains("small change"));
        assert_eq!(score(&input(10, &["billing/x.rs"]), &auto).0, Mode::Deep);
        assert_eq!(score(&input(10, &["svc/kms/key.go"]), &auto).0, Mode::Max);
        assert_eq!(score(&input(10, &["src/auth/token.rs"]), &auto).0, Mode::Deep);
        assert_eq!(score(&input(10, &["Cargo.lock"]), &auto).0, Mode::Standard);
        assert_eq!(score(&input(10, &["src/crypto/aes.rs"]), &auto).0, Mode::Max);
        assert_eq!(score(&input(2000, &["src/a.rs"]), &auto).0, Mode::Deep);
        assert_eq!(score(&input(10, &["a/x", "b/x", "c/x", "d/x", "e/x"]), &auto).0, Mode::Deep);
        let mut no_tests = input(300, &["src/a.rs"]);
        no_tests.tests_changed = false;
        assert_eq!(score(&no_tests, &auto).0, Mode::Deep);
        let mut docs = input(300, &["docs/a.md"]);
        docs.tests_changed = false;
        assert_eq!(score(&docs, &auto).0, Mode::Standard);
        let mut fan = input(10, &["src/a.rs"]);
        fan.fanout = 40;
        let (m, r) = score(&fan, &auto);
        assert_eq!(m, Mode::Deep);
        assert!(r[0].contains("40 callers"));
        let mut hist = input(10, &["src/a.rs"]);
        hist.author_reverts = 1;
        hist.author_ejections = 1;
        assert_eq!(score(&hist, &auto).0, Mode::Deep);
        let mut lab = input(10, &["src/a.rs"]);
        lab.labels = vec!["Security".into()];
        assert_eq!(score(&lab, &auto).0, Mode::Max);
        let mut owned = input(100, &["src/a.rs"]);
        owned.codeowned = 1;
        assert_eq!(score(&owned, &auto).0, Mode::Deep);
        // Several reasons are all listed; the deepest mode wins.
        let (m, r) = score(&input(2000, &["src/crypto/a.rs", "billing/b.rs"]), &auto);
        assert_eq!(m, Mode::Max);
        assert!(r.len() >= 3, "{r:?}");
        assert_eq!(ModeChoice::parse("auto"), Some(ModeChoice::Auto));
        assert_eq!(serde_json::to_value(ModeChoice::Fixed(Mode::Deep)).unwrap(), "deep");
    }
}
