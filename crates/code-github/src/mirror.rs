//! When to mirror a ref to GitHub. roll lands at a rate GitHub's merge APIs can't take, so the
//! landed head is pushed as one fast-forward that carries every commit since the last push. This
//! is the decision logic only: the caller owns the clock, does the git push and answers ancestry
//! questions, which keeps it deterministic and testable.
//!
//! ```text
//! landed(head) ──► poll() ──► Push { from, to } ──► push_succeeded(to)
//!                    │                          └─► push_failed(..) ──► backoff, poll() again
//!                    └─ Wait(d): debounce / backoff      Blocked: GitHub has commits roll doesn't
//! ```

use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct MirrorConfig {
    /// Push once landings have been quiet this long…
    pub debounce: Duration,
    /// …or once the oldest unpushed landing is this old, even if landings keep coming.
    pub max_delay: Duration,
    pub backoff_initial: Duration,
    pub backoff_max: Duration,
}

impl Default for MirrorConfig {
    fn default() -> Self {
        MirrorConfig {
            debounce: Duration::from_secs(2),
            max_delay: Duration::from_secs(10),
            backoff_initial: Duration::from_secs(1),
            backoff_max: Duration::from_secs(300),
        }
    }
}

/// Answers "is `ancestor` reachable from `descendant`?" over roll's history (git commit ids).
/// A commit counts as its own ancestor.
pub trait Ancestry {
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool;
}

impl<F: Fn(&str, &str) -> bool> Ancestry for F {
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> bool {
        self(ancestor, descendant)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MirrorAction {
    /// Nothing to push.
    Idle,
    /// Call `poll` again after this long (debounce or backoff).
    Wait(Duration),
    /// Push `to` to GitHub. `from` is the sha GitHub is expected to have (`None`: create the ref);
    /// use it as the old value of the ref update so a concurrent writer makes the push fail.
    Push { from: Option<String>, to: String },
    /// GitHub's ref holds commits roll doesn't have, so a fast-forward is impossible until the
    /// caller ingests them (`remote_ahead`) or someone resolves a real fork (`diverged`).
    Blocked(Blocked),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Blocked {
    /// What GitHub's ref points at.
    pub remote: String,
    pub kind: BlockedKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockedKind {
    /// GitHub is ahead of roll's head: ingest the remote commits, then `landed` + `resolve`.
    RemoteAhead,
    /// GitHub and roll each have commits the other doesn't.
    Diverged,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushFailure {
    /// Network error, 5xx, rate limit, a declined hook: retry with backoff.
    Transient(String),
    /// Not a fast-forward. `remote` is where GitHub's ref is now (`None`: it doesn't exist).
    Rejected { remote: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MirrorStatus {
    pub mirrored: Option<String>,
    pub head: Option<String>,
    pub in_flight: Option<String>,
    pub failures: u32,
    pub retry_in: Option<Duration>,
    pub blocked: Option<Blocked>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Mirror {
    config: MirrorConfig,
    /// What GitHub's ref is known to point at.
    mirrored: Option<String>,
    /// The latest landed head: where GitHub should end up.
    head: Option<String>,
    /// When the oldest landing not yet pushed (or in flight) happened.
    oldest_unpushed: Option<Instant>,
    last_landed: Option<Instant>,
    in_flight: Option<String>,
    /// First landing after the in-flight push was started.
    landed_during_flight: Option<Instant>,
    failures: u32,
    retry_at: Option<Instant>,
    blocked: Option<Blocked>,
    last_error: Option<String>,
}

impl Mirror {
    /// `mirrored`: GitHub's current sha for the ref, if known (e.g. right after import).
    pub fn new(config: MirrorConfig, mirrored: Option<String>) -> Self {
        Mirror {
            config,
            head: mirrored.clone(),
            mirrored,
            oldest_unpushed: None,
            last_landed: None,
            in_flight: None,
            landed_during_flight: None,
            failures: 0,
            retry_at: None,
            blocked: None,
            last_error: None,
        }
    }

    /// The ref advanced to `head` in roll (one landing or a whole batch).
    pub fn landed(&mut self, head: impl Into<String>, now: Instant) {
        let head = head.into();
        if self.head.as_deref() == Some(head.as_str()) {
            return;
        }
        self.head = Some(head);
        self.last_landed = Some(now);
        if self.in_flight.is_some() {
            self.landed_during_flight.get_or_insert(now);
        } else {
            self.oldest_unpushed.get_or_insert(now);
        }
    }

    pub fn poll(&mut self, now: Instant) -> MirrorAction {
        if let Some(b) = &self.blocked {
            return MirrorAction::Blocked(b.clone());
        }
        if self.in_flight.is_some() {
            return MirrorAction::Idle;
        }
        let Some(head) = self.head.clone() else {
            return MirrorAction::Idle;
        };
        if self.mirrored.as_deref() == Some(head.as_str()) {
            self.oldest_unpushed = None;
            return MirrorAction::Idle;
        }
        if let Some(at) = self.retry_at {
            if at > now {
                return MirrorAction::Wait(at - now);
            }
        } else {
            let oldest = *self.oldest_unpushed.get_or_insert(now);
            let quiet = self.last_landed.map_or(now, |t| t + self.config.debounce);
            let due = quiet.min(oldest + self.config.max_delay);
            if due > now {
                return MirrorAction::Wait(due - now);
            }
        }
        self.retry_at = None;
        self.in_flight = Some(head.clone());
        MirrorAction::Push {
            from: self.mirrored.clone(),
            to: head,
        }
    }

    pub fn push_succeeded(&mut self, to: &str, _now: Instant) {
        self.mirrored = Some(to.to_owned());
        self.finish_flight(true);
        self.failures = 0;
        self.retry_at = None;
        self.last_error = None;
    }

    pub fn push_failed(&mut self, failure: PushFailure, now: Instant, ancestry: &dyn Ancestry) {
        self.finish_flight(false);
        self.failures += 1;
        match failure {
            PushFailure::Transient(e) => {
                self.last_error = Some(e);
                self.retry_at = Some(now + self.backoff());
            }
            PushFailure::Rejected { remote } => {
                self.last_error = Some(format!(
                    "non-fast-forward; github is at {}",
                    remote.as_deref().unwrap_or("(no ref)")
                ));
                let before = self.mirrored.clone();
                self.remote_observed(remote.as_deref(), ancestry);
                // A stale view of GitHub (our last push landed but its response was lost) fixes
                // itself: retry now. Otherwise back off so a hook that keeps rejecting can't spin.
                let learned = self.mirrored != before;
                self.retry_at = Some(if learned { now } else { now + self.backoff() });
            }
        }
    }

    /// GitHub's ref was seen at `remote`: a push webhook, or the answer to a rejected push.
    pub fn remote_observed(&mut self, remote: Option<&str>, ancestry: &dyn Ancestry) {
        let Some(remote) = remote else {
            // The ref is gone on GitHub; the next push recreates it.
            self.mirrored = None;
            self.blocked = None;
            return;
        };
        if self.mirrored.as_deref() == Some(remote) && self.blocked.is_none() {
            return;
        }
        match self.head.as_deref() {
            None => self.mirrored = Some(remote.to_owned()),
            Some(head) if ancestry.is_ancestor(remote, head) => {
                self.mirrored = Some(remote.to_owned());
                self.blocked = None;
            }
            Some(head) => {
                let kind = if ancestry.is_ancestor(head, remote) {
                    BlockedKind::RemoteAhead
                } else {
                    BlockedKind::Diverged
                };
                self.blocked = Some(Blocked {
                    remote: remote.to_owned(),
                    kind,
                });
            }
        }
    }

    /// Re-check a block after the caller ingested GitHub's commits (and called `landed`) or
    /// otherwise reconciled. Returns whether pushing can resume.
    pub fn resolve(&mut self, ancestry: &dyn Ancestry) -> bool {
        let Some(b) = self.blocked.take() else {
            return true;
        };
        self.remote_observed(Some(&b.remote), ancestry);
        if self.blocked.is_none() {
            self.failures = 0;
            self.retry_at = None;
        }
        self.blocked.is_none()
    }

    pub fn status(&self, now: Instant) -> MirrorStatus {
        MirrorStatus {
            mirrored: self.mirrored.clone(),
            head: self.head.clone(),
            in_flight: self.in_flight.clone(),
            failures: self.failures,
            retry_in: self.retry_at.map(|at| at.saturating_duration_since(now)),
            blocked: self.blocked.clone(),
            last_error: self.last_error.clone(),
        }
    }

    fn finish_flight(&mut self, pushed: bool) {
        self.in_flight = None;
        let during = self.landed_during_flight.take();
        // After a successful push only landings made during it are unpushed; after a failure the
        // original oldest landing still is.
        if pushed {
            self.oldest_unpushed = during;
        }
        if self.head == self.mirrored {
            self.oldest_unpushed = None;
        }
    }

    fn backoff(&self) -> Duration {
        let exp = self.failures.saturating_sub(1).min(20);
        self.config
            .backoff_initial
            .saturating_mul(1 << exp)
            .min(self.config.backoff_max)
    }
}
