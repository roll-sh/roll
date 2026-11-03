//! Review rules: the repo's settings (stored in roll) and `.roll/review.yaml` (read from the
//! default branch's head, never from the change), and what they make of a change's reviews.
//!
//! The repo's settings are a floor the file can't lower: approvals take the larger count,
//! requirements add up, and `count_ai_approval` / `allow_self_approval` only hold when both
//! allow them. `auto_approve` rules from both apply.

use super::codeowners::{Codeowners, owner_matches};
use super::pattern::Pattern;
use super::{Reviewer, ReviewerKind, Verdict};
use crate::model::Blocker;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const FILE: &str = ".roll/review.yaml";
const MAX_BYTES: usize = 256 << 10;
pub const MAX_APPROVALS: u32 = 20;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutoApprove {
    /// Every changed path matches one of these (gitignore-style globs). Empty: any path.
    #[serde(default)]
    pub paths: Vec<String>,
    /// Only comments and whitespace changed (language-aware; unknown languages never are).
    #[serde(default)]
    pub comment_only: bool,
}

impl AutoApprove {
    pub fn describe(&self) -> String {
        match (self.paths.is_empty(), self.comment_only) {
            (false, false) => format!("paths: {}", self.paths.join(", ")),
            (true, true) => "comment_only".into(),
            (false, true) => format!("paths: {} (comment_only)", self.paths.join(", ")),
            (true, false) => "any change".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rules {
    /// Approvals a change needs (users, agents, and with `count_ai_approval` apps and roll).
    pub required_approvals: u32,
    /// Every CODEOWNERS group a change touches must approve.
    pub require_codeowners: bool,
    /// Apps' and roll's approvals count toward `required_approvals`.
    pub count_ai_approval: bool,
    /// A reviewer's "request changes" blocks until they approve, re-review or it's dismissed.
    pub block_on_request_changes: bool,
    /// Approvals of an older revision don't count.
    pub dismiss_stale: bool,
    /// Every thread must be resolved (outdated ones too).
    pub require_resolved_threads: bool,
    /// The change's author approving it counts.
    pub allow_self_approval: bool,
    /// Changes that need no approvals.
    pub auto_approve: Vec<AutoApprove>,
}

impl Default for Rules {
    fn default() -> Self {
        Rules {
            required_approvals: 0,
            require_codeowners: false,
            count_ai_approval: false,
            block_on_request_changes: true,
            dismiss_stale: true,
            require_resolved_threads: false,
            allow_self_approval: false,
            auto_approve: Vec::new(),
        }
    }
}

/// Some of the rules: a PATCH, or what `.roll/review.yaml` sets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RulesPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_approvals: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_codeowners: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count_ai_approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub block_on_request_changes: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dismiss_stale: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub require_resolved_threads: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allow_self_approval: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_approve: Option<Vec<AutoApprove>>,
}

impl RulesPatch {
    pub fn validate(&self) -> Result<(), String> {
        if let Some(n) = self.required_approvals
            && n > MAX_APPROVALS
        {
            return Err(format!("required_approvals must be 0-{MAX_APPROVALS}"));
        }
        if let Some(rules) = &self.auto_approve {
            if rules.len() > 50 {
                return Err("at most 50 auto_approve rules".into());
            }
            for r in rules {
                if r.paths.len() > 100 {
                    return Err("at most 100 paths per auto_approve rule".into());
                }
                if r.paths.is_empty() && !r.comment_only {
                    return Err("an auto_approve rule needs paths, comment_only or both".into());
                }
                if let Some(p) = r.paths.iter().find(|p| Pattern::new(p).is_none()) {
                    return Err(format!("auto_approve: empty path pattern {p:?}"));
                }
            }
        }
        Ok(())
    }

    /// Apply to stored settings (a PATCH).
    pub fn apply(&self, r: &mut Rules) {
        macro_rules! set {
            ($($f:ident),*) => { $( if let Some(v) = &self.$f { r.$f = v.clone(); } )* };
        }
        set!(required_approvals, require_codeowners, count_ai_approval, block_on_request_changes, dismiss_stale, require_resolved_threads, allow_self_approval, auto_approve);
    }

    /// Combine with the repo's settings: the file can only tighten them (see the module doc).
    pub fn tighten(&self, base: &Rules) -> Rules {
        let mut r = base.clone();
        if let Some(n) = self.required_approvals {
            r.required_approvals = r.required_approvals.max(n);
        }
        let or = |a: bool, b: Option<bool>| a || b.unwrap_or(false);
        let and = |a: bool, b: Option<bool>| a && b.unwrap_or(true);
        r.require_codeowners = or(r.require_codeowners, self.require_codeowners);
        r.block_on_request_changes = or(r.block_on_request_changes, self.block_on_request_changes);
        r.dismiss_stale = or(r.dismiss_stale, self.dismiss_stale);
        r.require_resolved_threads = or(r.require_resolved_threads, self.require_resolved_threads);
        r.count_ai_approval = and(r.count_ai_approval, self.count_ai_approval);
        r.allow_self_approval = and(r.allow_self_approval, self.allow_self_approval);
        if let Some(extra) = &self.auto_approve {
            r.auto_approve.extend(extra.iter().cloned());
        }
        r
    }
}

/// `.roll/review.yaml`, as read.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct ReviewFile {
    pub path: String,
    /// The rule keys it sets (`None` when it couldn't be read).
    pub rules: Option<RulesPatch>,
    /// The `reviewer:` section, for roll's reviewer, as written.
    pub reviewer: Option<Value>,
    /// Why it couldn't be used, or keys roll doesn't know.
    pub errors: Vec<String>,
}

pub fn parse_file(text: &str) -> ReviewFile {
    let mut f = ReviewFile { path: FILE.into(), ..Default::default() };
    if text.len() > MAX_BYTES {
        f.errors.push(format!("{FILE} is larger than {} KiB; ignored", MAX_BYTES >> 10));
        return f;
    }
    let value: serde_yaml_ng::Value = match serde_yaml_ng::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            f.errors.push(format!("{FILE}: {e}"));
            return f;
        }
    };
    let map = match value {
        serde_yaml_ng::Value::Mapping(m) => m,
        serde_yaml_ng::Value::Null => {
            f.rules = Some(RulesPatch::default());
            return f;
        }
        _ => {
            f.errors.push(format!("{FILE}: expected a mapping of settings"));
            return f;
        }
    };
    let mut known = serde_yaml_ng::Mapping::new();
    for (k, v) in map {
        let key = k.as_str().unwrap_or("").to_string();
        match key.as_str() {
            "reviewer" => f.reviewer = serde_json::to_value(&v).ok(),
            "required_approvals" | "require_codeowners" | "count_ai_approval" | "block_on_request_changes" | "dismiss_stale"
            | "require_resolved_threads" | "allow_self_approval" | "auto_approve" => {
                known.insert(k, v);
            }
            _ => f.errors.push(format!("{FILE}: unknown key {key:?} (ignored)")),
        }
    }
    match serde_yaml_ng::from_value::<RulesPatch>(serde_yaml_ng::Value::Mapping(known)) {
        Ok(p) => match p.validate() {
            Ok(()) => f.rules = Some(p),
            Err(e) => f.errors.push(format!("{FILE}: {e}")),
        },
        Err(e) => f.errors.push(format!("{FILE}: {e}")),
    }
    f
}

/// What the evaluation needs to know about a change's diff.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    /// Paths changed between the change's merge base with the default branch and its head.
    pub paths: Vec<String>,
    /// Whether every changed file is comment-only; `None` until known.
    pub comment_only: Option<bool>,
}

/// A review as the rules see it.
#[derive(Clone, Debug)]
pub struct Rec {
    pub id: String,
    pub reviewer: Reviewer,
    pub verdict: Verdict,
    pub revision: u64,
    pub created_at: i64,
    pub seq: i64,
    /// A dismissed review still supersedes the reviewer's earlier ones, but counts for nothing:
    /// dismissing someone's latest change request must not bring back an older one.
    pub dismissed: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Approval {
    pub reviewer: Reviewer,
    pub review: String,
    pub stale: bool,
    /// Counts toward `required_approvals`.
    pub counted: bool,
    /// Why it doesn't count.
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct OwnerGroup {
    pub path_pattern: String,
    pub owners: Vec<String>,
    pub paths: Vec<String>,
    pub satisfied: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct Status {
    /// `approved`, `changes_requested`, `pending` or `auto_approved`.
    pub state: &'static str,
    pub required: u32,
    pub approvals: Vec<Approval>,
    pub codeowners: Vec<OwnerGroup>,
    pub changes_requested: Vec<Reviewer>,
    pub unresolved_threads: u64,
    pub pending: Vec<Reviewer>,
    pub blockers: Vec<Blocker>,
    pub auto_approved: Option<Value>,
}

pub struct Input<'a> {
    pub rules: &'a Rules,
    pub codeowners: &'a Codeowners,
    pub facts: &'a Facts,
    /// The change's author (as recorded on the change).
    pub author: &'a str,
    pub revision: u64,
    /// Non-dismissed reviews, any order.
    pub reviews: &'a [Rec],
    pub unresolved_threads: u64,
    /// Required reviews not given yet for the current head.
    pub pending: &'a [Reviewer],
}

fn is_ai(r: &Reviewer) -> bool {
    matches!(r.kind, ReviewerKind::App | ReviewerKind::Roll)
}

pub fn evaluate(i: &Input) -> Status {
    let rules = i.rules;
    // Each reviewer's latest decisive (approve / request changes) review.
    let mut latest: Vec<&Rec> = Vec::new();
    let mut sorted: Vec<&Rec> = i.reviews.iter().collect();
    sorted.sort_by_key(|r| (r.created_at, r.seq));
    for r in sorted {
        if r.verdict == Verdict::Comment {
            continue;
        }
        match latest.iter_mut().find(|l| l.reviewer.same(&r.reviewer)) {
            Some(slot) => *slot = r,
            None => latest.push(r),
        }
    }
    let self_review = |r: &Reviewer| r.name.eq_ignore_ascii_case(i.author) || r.name.eq_ignore_ascii_case(i.author.trim_start_matches('@'));
    let mut approvals = Vec::new();
    let mut changes_requested = Vec::new();
    for r in latest.iter().filter(|r| !r.dismissed) {
        match r.verdict {
            Verdict::Approve => {
                let stale = r.revision < i.revision;
                let reason = if stale && rules.dismiss_stale {
                    Some("approved an older revision")
                } else if self_review(&r.reviewer) && !rules.allow_self_approval {
                    Some("the author can't approve their own change")
                } else if is_ai(&r.reviewer) && !rules.count_ai_approval {
                    Some("AI approvals don't count toward required approvals (count_ai_approval)")
                } else {
                    None
                };
                approvals.push(Approval {
                    reviewer: r.reviewer.clone(),
                    review: r.id.clone(),
                    stale,
                    counted: reason.is_none(),
                    reason: reason.map(str::to_string),
                });
            }
            Verdict::RequestChanges => changes_requested.push(r.reviewer.clone()),
            Verdict::Comment => {}
        }
    }
    // Approvals that can satisfy a code owner: current (or stale allowed) and not the author's.
    // An owner that names an app (app:x) or roll can be satisfied by it whatever count_ai says.
    let owner_ok: Vec<&Approval> = approvals
        .iter()
        .filter(|a| !(a.stale && rules.dismiss_stale) && (!self_review(&a.reviewer) || rules.allow_self_approval))
        .collect();
    let mut groups: Vec<OwnerGroup> = Vec::new();
    for path in &i.facts.paths {
        let Some(rule) = i.codeowners.rule_for(path) else { continue };
        if rule.owners.is_empty() {
            continue;
        }
        match groups.iter_mut().find(|g| g.path_pattern == rule.pattern && g.owners == rule.owners) {
            Some(g) => g.paths.push(path.clone()),
            None => groups.push(OwnerGroup {
                path_pattern: rule.pattern.clone(),
                owners: rule.owners.clone(),
                paths: vec![path.clone()],
                satisfied: false,
            }),
        }
    }
    for g in &mut groups {
        g.satisfied = owner_ok.iter().any(|a| g.owners.iter().any(|o| owner_matches(o, &a.reviewer)));
    }

    let auto = rules.auto_approve.iter().find(|a| {
        let paths_ok = a.paths.is_empty() || {
            let pats: Vec<Pattern> = a.paths.iter().filter_map(|p| Pattern::new(p)).collect();
            i.facts.paths.iter().all(|p| super::pattern::any_match(&pats, p))
        };
        let comments_ok = !a.comment_only || i.facts.comment_only == Some(true);
        paths_ok && comments_ok
    });

    let counted = approvals.iter().filter(|a| a.counted).count() as u32;
    let mut blockers = Vec::new();
    if auto.is_none() {
        if counted < rules.required_approvals {
            blockers.push(Blocker::new(
                "needs_approval",
                format!(
                    "needs {} approval{} ({counted} so far)",
                    rules.required_approvals,
                    if rules.required_approvals == 1 { "" } else { "s" }
                ),
            ));
        }
        if rules.require_codeowners {
            for g in groups.iter().filter(|g| !g.satisfied) {
                blockers.push(Blocker::new(
                    "needs_codeowner",
                    format!("needs approval from {} (code owners of {})", g.owners.join(" or "), g.path_pattern),
                ));
            }
        }
    }
    if rules.block_on_request_changes && !changes_requested.is_empty() {
        let who: Vec<String> = changes_requested.iter().map(Reviewer::display).collect();
        blockers.push(Blocker::new("changes_requested", format!("{} requested changes", who.join(", "))));
    }
    if rules.require_resolved_threads && i.unresolved_threads > 0 {
        let n = i.unresolved_threads;
        blockers.push(Blocker::new("unresolved_threads", format!("{n} unresolved thread{}", if n == 1 { "" } else { "s" })));
    }
    if !i.pending.is_empty() {
        let who: Vec<String> = i.pending.iter().map(Reviewer::display).collect();
        blockers.push(Blocker::new("review_pending", format!("waiting for a review from {}", who.join(", "))));
    }
    let state = if !changes_requested.is_empty() && rules.block_on_request_changes {
        "changes_requested"
    } else if auto.is_some() {
        "auto_approved"
    } else if blockers.iter().any(|b| b.kind == "needs_approval" || b.kind == "needs_codeowner") {
        "pending"
    } else if counted > 0 || (rules.require_codeowners && groups.iter().any(|g| g.satisfied)) {
        "approved"
    } else {
        // Nothing approved it (and maybe nothing had to).
        "pending"
    };
    Status {
        state,
        required: rules.required_approvals,
        approvals,
        codeowners: groups,
        changes_requested,
        unresolved_threads: i.unresolved_threads,
        pending: i.pending.to_vec(),
        blockers,
        auto_approved: auto.map(|a| serde_json::json!({ "rule": a.describe() })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rev(kind: ReviewerKind, name: &str) -> Reviewer {
        Reviewer { kind, name: name.into(), app: None }
    }

    fn rec(n: i64, r: Reviewer, verdict: Verdict, revision: u64) -> Rec {
        Rec { id: format!("rv_{n}"), reviewer: r, verdict, revision, created_at: n, seq: n, dismissed: false }
    }

    #[test]
    fn a_dismissed_review_doesnt_bring_back_an_older_one() {
        use ReviewerKind::*;
        let rules = Rules::default();
        let none = Codeowners::default();
        let old = rec(1, rev(Roll, "roll"), Verdict::RequestChanges, 1);
        let latest = Rec { dismissed: true, ..rec(2, rev(Roll, "roll"), Verdict::RequestChanges, 2) };
        let s = eval(&rules, &none, &["a"], &[old.clone(), latest]);
        assert!(s.changes_requested.is_empty(), "{:?}", s.blockers);
        assert!(s.blockers.is_empty(), "{:?}", s.blockers);
        // A dismissed approval doesn't count either, and doesn't revive an older approval.
        let approve = rec(3, rev(User, "bob"), Verdict::Approve, 2);
        let dismissed = Rec { dismissed: true, ..rec(4, rev(User, "bob"), Verdict::Approve, 2) };
        let req = Rules { required_approvals: 1, ..Rules::default() };
        assert_eq!(kinds(&eval(&req, &none, &["a"], &[approve, dismissed])), vec!["needs_approval"]);
        // Without a newer review, the older request still blocks.
        assert_eq!(kinds(&eval(&rules, &none, &["a"], &[old])), vec!["changes_requested"]);
    }

    fn eval(rules: &Rules, owners: &Codeowners, paths: &[&str], reviews: &[Rec]) -> Status {
        let facts = Facts { paths: paths.iter().map(|s| s.to_string()).collect(), comment_only: Some(false) };
        evaluate(&Input { rules, codeowners: owners, facts: &facts, author: "carol", revision: 2, reviews, unresolved_threads: 0, pending: &[] })
    }

    fn kinds(s: &Status) -> Vec<&str> {
        s.blockers.iter().map(|b| b.kind.as_str()).collect()
    }

    #[test]
    fn approvals_count() {
        use ReviewerKind::*;
        let rules = Rules { required_approvals: 2, ..Rules::default() };
        let none = Codeowners::default();
        let s = eval(&rules, &none, &["a.rs"], &[]);
        assert_eq!(kinds(&s), vec!["needs_approval"]);
        assert_eq!(s.state, "pending");
        let s = eval(&rules, &none, &["a.rs"], &[rec(1, rev(User, "alice"), Verdict::Approve, 2), rec(2, rev(Agent, "bot"), Verdict::Approve, 2)]);
        assert!(s.blockers.is_empty(), "{:?}", s.blockers);
        assert_eq!(s.state, "approved");
        // The same reviewer twice is one approval.
        let s = eval(&rules, &none, &["a.rs"], &[rec(1, rev(User, "alice"), Verdict::Approve, 2), rec(2, rev(User, "alice"), Verdict::Approve, 2)]);
        assert_eq!(kinds(&s), vec!["needs_approval"]);
        // Comment reviews don't change a verdict.
        let s = eval(&rules, &none, &["a.rs"], &[rec(1, rev(User, "alice"), Verdict::Approve, 2), rec(2, rev(User, "alice"), Verdict::Comment, 2), rec(3, rev(User, "bob"), Verdict::Approve, 2)]);
        assert!(s.blockers.is_empty());
    }

    #[test]
    fn stale_self_and_ai_approvals() {
        use ReviewerKind::*;
        let rules = Rules { required_approvals: 1, ..Rules::default() };
        let none = Codeowners::default();
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::Approve, 1)]);
        assert_eq!(kinds(&s), vec!["needs_approval"], "dismiss_stale: an older revision's approval doesn't count");
        assert!(s.approvals[0].stale && !s.approvals[0].counted);
        let keep = Rules { dismiss_stale: false, ..rules.clone() };
        assert!(eval(&keep, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::Approve, 1)]).blockers.is_empty());
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(User, "carol"), Verdict::Approve, 2)]);
        assert_eq!(kinds(&s), vec!["needs_approval"], "self-approval");
        let selfok = Rules { allow_self_approval: true, ..rules.clone() };
        assert!(eval(&selfok, &none, &["a"], &[rec(1, rev(User, "carol"), Verdict::Approve, 2)]).blockers.is_empty());
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(App, "coderabbitai[bot]"), Verdict::Approve, 2), rec(2, rev(Roll, "roll"), Verdict::Approve, 2)]);
        assert_eq!(kinds(&s), vec!["needs_approval"], "AI approvals don't count by default");
        let ai = Rules { count_ai_approval: true, ..rules.clone() };
        assert!(eval(&ai, &none, &["a"], &[rec(1, rev(App, "greptile"), Verdict::Approve, 2)]).blockers.is_empty());
    }

    #[test]
    fn request_changes_blocks_until_superseded() {
        use ReviewerKind::*;
        let rules = Rules::default();
        let none = Codeowners::default();
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::RequestChanges, 1)]);
        assert_eq!(kinds(&s), vec!["changes_requested"], "even from an older revision");
        assert_eq!(s.state, "changes_requested");
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::RequestChanges, 1), rec(2, rev(User, "alice"), Verdict::Approve, 2)]);
        assert!(s.blockers.is_empty());
        let s = eval(&rules, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::RequestChanges, 1), rec(2, rev(User, "alice"), Verdict::Comment, 2)]);
        assert_eq!(kinds(&s), vec!["changes_requested"], "a comment doesn't lift it");
        let off = Rules { block_on_request_changes: false, ..rules };
        assert!(eval(&off, &none, &["a"], &[rec(1, rev(User, "alice"), Verdict::RequestChanges, 2)]).blockers.is_empty());
    }

    #[test]
    fn codeowners_groups() {
        use ReviewerKind::*;
        let owners = Codeowners::parse("CODEOWNERS", "* @lead\n/crates/ @rust agent:fixer\n/docs/\n");
        let rules = Rules { require_codeowners: true, ..Rules::default() };
        let s = eval(&rules, &owners, &["crates/a.rs", "crates/b.rs", "README.md", "docs/x.md"], &[]);
        assert_eq!(s.codeowners.len(), 2, "{:?}", s.codeowners);
        assert_eq!(s.codeowners[0].paths, vec!["crates/a.rs", "crates/b.rs"]);
        assert_eq!(kinds(&s), vec!["needs_codeowner", "needs_codeowner"]);
        let s = eval(&rules, &owners, &["crates/a.rs", "README.md"], &[rec(1, rev(Agent, "fixer"), Verdict::Approve, 2), rec(2, rev(User, "lead"), Verdict::Approve, 2)]);
        assert!(s.blockers.is_empty(), "{:?}", s.blockers);
        // The author can't satisfy their own code-owner requirement.
        let s = eval(&rules, &Codeowners::parse("CODEOWNERS", "* @carol\n"), &["a"], &[rec(1, rev(User, "carol"), Verdict::Approve, 2)]);
        assert_eq!(kinds(&s), vec!["needs_codeowner"]);
        // A code owner that is an app is satisfied by it even without count_ai_approval.
        let s = eval(&rules, &Codeowners::parse("CODEOWNERS", "* app:greptile\n"), &["a"], &[rec(1, rev(App, "greptile"), Verdict::Approve, 2)]);
        assert!(s.blockers.is_empty());
        // Unowned paths need nobody.
        assert!(eval(&rules, &owners, &["docs/x.md"], &[]).blockers.is_empty());
    }

    #[test]
    fn auto_approve() {
        let rules = Rules {
            required_approvals: 1,
            require_codeowners: true,
            auto_approve: vec![AutoApprove { paths: vec!["docs/**".into(), "**/*.md".into()], comment_only: false }],
            ..Rules::default()
        };
        let owners = Codeowners::parse("CODEOWNERS", "* @lead\n");
        let s = eval(&rules, &owners, &["docs/a.md", "README.md"], &[]);
        assert!(s.blockers.is_empty());
        assert_eq!(s.state, "auto_approved");
        assert_eq!(s.auto_approved.unwrap()["rule"], "paths: docs/**, **/*.md");
        assert_eq!(kinds(&eval(&rules, &owners, &["docs/a.md", "src/a.rs"], &[])), vec!["needs_approval", "needs_codeowner"]);
        let comments = Rules { auto_approve: vec![AutoApprove { paths: vec![], comment_only: true }], ..rules.clone() };
        let facts = Facts { paths: vec!["a.rs".into()], comment_only: Some(true) };
        let s = evaluate(&Input { rules: &comments, codeowners: &owners, facts: &facts, author: "x", revision: 1, reviews: &[], unresolved_threads: 0, pending: &[] });
        assert_eq!(s.state, "auto_approved");
        let unknown = Facts { paths: vec!["a.rs".into()], comment_only: None };
        let s = evaluate(&Input { rules: &comments, codeowners: &owners, facts: &unknown, author: "x", revision: 1, reviews: &[], unresolved_threads: 0, pending: &[] });
        assert_eq!(s.state, "pending", "not known to be comment-only: not auto-approved");
        // Requested changes still block an auto-approved change.
        let s = eval(&rules, &owners, &["docs/a.md"], &[rec(1, rev(ReviewerKind::User, "z"), Verdict::RequestChanges, 2)]);
        assert_eq!(kinds(&s), vec!["changes_requested"]);
    }

    #[test]
    fn threads_and_pending() {
        let rules = Rules { require_resolved_threads: true, ..Rules::default() };
        let none = Codeowners::default();
        let facts = Facts::default();
        let pending = [rev(ReviewerKind::Roll, "roll")];
        let s = evaluate(&Input { rules: &rules, codeowners: &none, facts: &facts, author: "x", revision: 1, reviews: &[], unresolved_threads: 2, pending: &pending });
        assert_eq!(kinds(&s), vec!["unresolved_threads", "review_pending"]);
        assert!(s.blockers[0].message.starts_with("2 unresolved threads"));
    }

    #[test]
    fn file_tightens_settings() {
        let base = Rules { required_approvals: 2, count_ai_approval: true, ..Rules::default() };
        let f = parse_file("required_approvals: 1\ncount_ai_approval: true\nrequire_codeowners: true\ndismiss_stale: false\nauto_approve:\n  - paths: [\"docs/**\"]\nreviewer:\n  mode: auto\nbogus: 1\n");
        assert_eq!(f.errors.len(), 1, "{:?}", f.errors);
        assert!(f.errors[0].contains("bogus"));
        assert_eq!(f.reviewer.as_ref().unwrap()["mode"], "auto");
        let r = f.rules.unwrap().tighten(&base);
        assert_eq!(r.required_approvals, 2, "the file can't lower approvals");
        assert!(r.require_codeowners);
        assert!(r.dismiss_stale, "the file can't turn dismiss_stale off");
        assert!(r.count_ai_approval);
        assert_eq!(r.auto_approve.len(), 1);
        let no_ai = parse_file("count_ai_approval: false\n").rules.unwrap().tighten(&base);
        assert!(!no_ai.count_ai_approval);
        let bad = parse_file("required_approvals: lots\n");
        assert!(bad.rules.is_none() && !bad.errors.is_empty());
        let bad = parse_file("required_approvals: 99\n");
        assert!(bad.rules.is_none() && bad.errors[0].contains("0-20"));
        let bad = parse_file("auto_approve:\n  - {}\n");
        assert!(bad.rules.is_none(), "{:?}", bad.errors);
        assert!(parse_file("- a\n").errors[0].contains("mapping"));
        assert_eq!(parse_file("").rules, Some(RulesPatch::default()));
        assert!(parse_file(&"#".repeat(MAX_BYTES + 1)).errors[0].contains("larger"));
    }
}
