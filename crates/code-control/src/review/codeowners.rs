//! CODEOWNERS, in GitHub's syntax: `pattern owner…` per line, `#` comments, the last matching
//! line wins, and a pattern with no owners un-owns its paths. Read from the default branch's
//! head: `CODEOWNERS`, `.roll/CODEOWNERS`, `.github/CODEOWNERS` or `docs/CODEOWNERS` (the first
//! that exists).
//!
//! Owners, and the reviewers they stand for:
//! - `@name`: the roll user (or org token) called `name`; for GitHub-connected repos, the GitHub
//!   login `name`.
//! - `@org/team`: roll has no teams yet, so the org's own token (`org`) or a reviewer literally
//!   called `org/team`.
//! - `agent:name`: the agent `name` (a writer's token sent with `X-Roll-Agent: name`).
//! - `app:name`: the review app `name` (or the GitHub App `name[bot]`); `roll` for roll's reviewer.
//! - `user@example.com`: only a reviewer with that exact name (roll knows no emails).

use super::Reviewer;
use super::pattern::Pattern;
use serde::Serialize;

pub const PATHS: [&str; 4] = ["CODEOWNERS", ".roll/CODEOWNERS", ".github/CODEOWNERS", "docs/CODEOWNERS"];
const MAX_BYTES: usize = 3 << 20;
const MAX_RULES: usize = 5000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Rule {
    pub pattern: String,
    pub owners: Vec<String>,
    /// 1-based line in the file.
    pub line: usize,
    #[serde(skip)]
    matcher: Pattern,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Codeowners {
    /// Where it was read from; `None` without a file.
    pub path: Option<String>,
    pub rules: Vec<Rule>,
    /// Lines that couldn't be used, as `line N: why`.
    pub errors: Vec<String>,
}

impl Codeowners {
    pub fn parse(path: &str, text: &str) -> Codeowners {
        let mut out = Codeowners { path: Some(path.to_string()), ..Default::default() };
        if text.len() > MAX_BYTES {
            out.errors.push(format!("{path} is larger than {} MiB; ignored", MAX_BYTES >> 20));
            return out;
        }
        for (i, raw) in text.lines().enumerate() {
            let n = i + 1;
            let line = strip_comment(raw);
            let mut words = line.split_whitespace();
            let Some(pattern) = words.next() else { continue };
            if out.rules.len() >= MAX_RULES {
                out.errors.push(format!("line {n}: more than {MAX_RULES} rules; the rest are ignored"));
                break;
            }
            let pattern = pattern.replace("\\#", "#");
            if pattern.starts_with('!') {
                out.errors.push(format!("line {n}: negated patterns aren't supported in CODEOWNERS"));
                continue;
            }
            let Some(matcher) = Pattern::new(&pattern) else {
                out.errors.push(format!("line {n}: empty pattern"));
                continue;
            };
            let mut owners = Vec::new();
            for w in words {
                if valid_owner(w) {
                    owners.push(w.to_string());
                } else {
                    out.errors.push(format!("line {n}: {w:?} isn't an owner (use @name, @org/team, agent:name or app:name)"));
                }
            }
            out.rules.push(Rule { pattern, owners, line: n, matcher });
        }
        out
    }

    /// The rule that owns `path` (the last that matches).
    pub fn rule_for(&self, path: &str) -> Option<&Rule> {
        self.rules.iter().rev().find(|r| r.matcher.matches(path))
    }
}

/// `#` starts a comment unless escaped (`\#`).
fn strip_comment(line: &str) -> &str {
    let b = line.as_bytes();
    for i in 0..b.len() {
        if b[i] == b'#' && (i == 0 || b[i - 1] != b'\\') {
            return &line[..i];
        }
    }
    line
}

fn valid_owner(w: &str) -> bool {
    let ok = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./[]".contains(c));
    if let Some(rest) = w.strip_prefix('@') {
        return match rest.split_once('/') {
            Some((org, team)) => ok(org) && ok(team),
            None => ok(rest),
        };
    }
    if let Some(rest) = w.strip_prefix("agent:").or_else(|| w.strip_prefix("app:")) {
        return ok(rest);
    }
    if w == "roll" {
        return true;
    }
    // An email address.
    w.split_once('@').is_some_and(|(a, d)| !a.is_empty() && d.contains('.'))
}

/// Does `owner` (as written in CODEOWNERS) stand for `r`?
pub fn owner_matches(owner: &str, r: &Reviewer) -> bool {
    use super::ReviewerKind as K;
    let eq = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
    if let Some(name) = owner.strip_prefix("agent:") {
        return r.kind == K::Agent && eq(&r.name, name);
    }
    if let Some(name) = owner.strip_prefix("app:") {
        let bot = r.name.strip_suffix("[bot]").unwrap_or(&r.name);
        return (r.kind == K::App && (eq(&r.name, name) || eq(bot, name))) || (r.kind == K::Roll && eq(name, "roll"));
    }
    if owner == "roll" {
        return r.kind == K::Roll;
    }
    if let Some(rest) = owner.strip_prefix('@') {
        if r.kind != K::User {
            return false;
        }
        return match rest.split_once('/') {
            Some((org, _)) => eq(&r.name, org) || eq(&r.name, rest),
            None => eq(&r.name, rest),
        };
    }
    r.kind == K::User && eq(&r.name, owner)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::review::ReviewerKind;

    fn rev(kind: ReviewerKind, name: &str) -> Reviewer {
        Reviewer { kind, name: name.into(), app: None }
    }

    #[test]
    fn last_match_wins_and_unowning() {
        let c = Codeowners::parse(
            "CODEOWNERS",
            "# owners\n*       @everyone\n*.rs    @rustaceans agent:fixer\n/docs/  @writers   # docs team\n/docs/generated/\nsrc/\\#odd @odd\n",
        );
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.rule_for("README.md").unwrap().owners, vec!["@everyone"]);
        assert_eq!(c.rule_for("src/lib.rs").unwrap().owners, vec!["@rustaceans", "agent:fixer"]);
        assert_eq!(c.rule_for("docs/guide.md").unwrap().owners, vec!["@writers"]);
        assert!(c.rule_for("docs/generated/api.md").unwrap().owners.is_empty(), "a rule without owners un-owns");
        assert_eq!(c.rule_for("docs/x.rs").unwrap().owners, vec!["@writers"], "the last match wins, not the most specific");
        assert_eq!(c.rule_for("src/#odd").unwrap().owners, vec!["@odd"]);
        assert_eq!(c.rule_for("docs/guide.md").unwrap().line, 4);
    }

    #[test]
    fn errors_are_reported_per_line() {
        let c = Codeowners::parse("CODEOWNERS", "*.js @ok bogus\n!*.md @x\n");
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.rules[0].owners, vec!["@ok"]);
        assert_eq!(c.errors.len(), 2, "{:?}", c.errors);
        assert!(c.errors[0].starts_with("line 1"));
        assert!(c.errors[1].contains("negated"));
        let big = Codeowners::parse("CODEOWNERS", &"#".repeat(MAX_BYTES + 1));
        assert!(big.rules.is_empty() && big.errors[0].contains("larger"));
    }

    #[test]
    fn owners_stand_for_reviewers() {
        use ReviewerKind::*;
        assert!(owner_matches("@alice", &rev(User, "Alice")));
        assert!(!owner_matches("@alice", &rev(Agent, "alice")), "@name is a person, not an agent");
        assert!(owner_matches("@acme/core", &rev(User, "acme")));
        assert!(owner_matches("@acme/core", &rev(User, "acme/core")));
        assert!(!owner_matches("@acme/core", &rev(User, "core")));
        assert!(owner_matches("agent:fixer", &rev(Agent, "fixer")));
        assert!(!owner_matches("agent:fixer", &rev(User, "fixer")));
        assert!(owner_matches("app:coderabbitai", &rev(App, "coderabbitai[bot]")));
        assert!(owner_matches("app:greptile", &rev(App, "greptile")));
        assert!(owner_matches("app:roll", &rev(Roll, "roll")));
        assert!(owner_matches("roll", &rev(Roll, "roll")));
        assert!(!owner_matches("roll", &rev(User, "roll")));
        assert!(owner_matches("dev@example.com", &rev(User, "dev@example.com")));
        assert!(!owner_matches("dev@example.com", &rev(User, "dev")));
    }
}
