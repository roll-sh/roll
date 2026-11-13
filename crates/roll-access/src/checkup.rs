//! The access checkup (docs/permissions.md, "Checkup"): which live tokens need a person's
//! attention and why, as Google's security checkup and GitHub's security overview point at what to
//! fix. Each finding names its fix; the dashboard puts it one click away.

use crate::store::{Grant, ROAMING};
use roll_auth::scope::{Action, Effect, Resources, Scope};
use serde::Serialize;

/// How long without a request makes a token unused.
pub const UNUSED_DAYS: i64 = 30;

/// One thing to look at, most serious first by kind.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Finding {
    /// `everything`, `never_expires`, `unused`, `narrower` or `roams`.
    pub kind: &'static str,
    /// In words, for the person.
    pub why: String,
    /// What fixes it: `tighten`, `expire`, `revoke` or `limit_addresses`.
    pub fix: &'static str,
}

/// What's known about a grant beyond its record: the networks it's been used from, and whether
/// its tighten proposal ([`crate::tighten`]) is narrower than its rules.
pub struct Seen {
    pub networks: i64,
    pub narrower: bool,
}

/// A grant's findings, most serious first; none for one that isn't live.
pub fn findings(g: &Grant, seen: &Seen, now: i64) -> Vec<Finding> {
    if !g.live(now) {
        return Vec::new();
    }
    let mut out = Vec::new();
    let day = 86_400_000;
    let strong = [Action::Admin, Action::Reveal, Action::Delete];
    let everything = |s: &Scope| {
        s.0.iter().any(|r| r.product == "*" && r.resources == Resources::All && r.environments.is_none() && strong.iter().any(|a| r.actions.get(a) == Some(&Effect::Allow)))
    };
    if everything(&g.rules) {
        out.push(Finding { kind: "everything", why: "it may delete, reveal or administer every product's resources without asking".into(), fix: "tighten" });
    }
    if g.expires_at.is_none() {
        out.push(Finding { kind: "never_expires", why: "it never expires".into(), fix: "expire" });
    }
    let last = g.last_used_at.unwrap_or(g.created_at);
    if now - last > UNUSED_DAYS * day {
        let since = if g.last_used_at.is_some() { "made no request" } else { "never made a request" };
        out.push(Finding { kind: "unused", why: format!("it {since} in {} days", (now - last) / day), fix: "revoke" });
    } else if seen.narrower {
        out.push(Finding { kind: "narrower", why: format!("its requests in the last {UNUSED_DAYS} days needed less than its rules allow"), fix: "tighten" });
    }
    if seen.networks >= ROAMING && g.ip_allow.is_empty() {
        out.push(Finding { kind: "roams", why: format!("it's been used from {} networks, with no address limit", seen.networks), fix: "limit_addresses" });
    }
    out
}
