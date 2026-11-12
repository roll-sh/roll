//! Leaked secrets don't land (docs/secrets.md): a change whose added lines hold the value of a
//! secret in its repo's workspace project is blocked with `secret_leak`. The lines a change adds
//! since its merge base are cut into candidate tokens, roll secrets matches them by fingerprint
//! ([`crate::SecretEnv::leaks`]), and the answer is cached per (repo, head) with the merge base it
//! was found from, as review facts are. Blockers name keys and environments, never a value or a line.

use crate::model::{Blocker, Change, ChangeState, RepoRow};
use crate::{Code, Result};
use code_store::{ChangesetId, Mode};
use std::collections::{BTreeMap, HashSet};
use std::sync::Arc;

/// The most candidate tokens checked per change.
const MAX_CANDIDATES: usize = 20_000;
/// Added lines longer than this (bytes) are skipped: minified or generated text.
const MAX_LINE: usize = 4096;
/// Files larger than this (bytes, either side) aren't diffed.
const MAX_FILE: u64 = 1 << 20;

/// (key, environment) of every secret whose value a change adds.
pub(crate) type Leaks = Arc<Vec<(String, String)>>;

impl Code {
    /// Make sure [`leak_blockers`](Self::leak_blockers) knows whether `c`'s head adds a
    /// secret's value. Nothing without roll secrets; an error is logged and tried again next time.
    pub(crate) async fn prepare_leaks(&self, r: &RepoRow, c: &Change) {
        let Some(secrets) = self.secret_env.get().cloned() else { return };
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) {
            return;
        }
        let Ok(Some(head)) = self.store.resolve(&r.id, &c.head) else { return };
        let Ok(mb) = self.merge_base(r, &head) else { return };
        let key = (r.id.clone(), head);
        if self.leak_facts.get(&key).is_some_and(|(at, _)| at == mb) {
            return;
        }
        let found = match self.added_candidates(r, mb.as_ref(), &head).await {
            Ok(candidates) if candidates.is_empty() => Ok(Vec::new()),
            Ok(candidates) => secrets.leaks(&r.full_name(), candidates).await,
            Err(e) => Err(e.into()),
        };
        match found {
            Ok(mut leaks) => {
                leaks.sort();
                leaks.dedup();
                self.leak_facts.put(key, (mb, Arc::new(leaks)));
            }
            Err(e) => tracing::warn!(change = %c.id, error = %e, "checking a change for leaked secrets"),
        }
    }

    /// One `secret_leak` blocker per key whose value `c` adds (from what
    /// [`prepare_leaks`](Self::prepare_leaks) last found for its head).
    pub(crate) fn leak_blockers(&self, r: &RepoRow, c: &Change) -> Vec<Blocker> {
        if matches!(c.state, ChangeState::Landed | ChangeState::Closed) || self.secret_env.get().is_none() {
            return Vec::new();
        }
        let Ok(Some(head)) = self.store.resolve(&r.id, &c.head) else { return Vec::new() };
        let Some((_, leaks)) = self.leak_facts.get(&(r.id.clone(), head)) else { return Vec::new() };
        let mut by_key: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (key, env) in leaks.iter() {
            by_key.entry(key).or_default().push(env);
        }
        by_key
            .into_iter()
            .map(|(key, envs)| Blocker::new("secret_leak", format!("adds the value of {key} ({}): rotate it and remove it from the change", envs.join(", "))))
            .collect()
    }

    /// The candidate tokens of the lines `head` adds since `mb`, bounded by [`MAX_CANDIDATES`].
    /// Binary files, files over [`MAX_FILE`] and lines over [`MAX_LINE`] are skipped.
    async fn added_candidates(&self, r: &RepoRow, mb: Option<&ChangesetId>, head: &ChangesetId) -> Result<Vec<String>> {
        let mut out = HashSet::new();
        for d in self.changed_paths(r, mb, head)? {
            if out.len() >= MAX_CANDIDATES {
                break;
            }
            if d.new_oid.is_none() || d.new_mode == Some(Mode::Gitlink) {
                continue;
            }
            let size = |oid| self.store.blob_size(&r.id, &oid).ok().flatten().unwrap_or(u64::MAX);
            if [d.old_oid, d.new_oid].into_iter().flatten().any(|o| size(o) > MAX_FILE) {
                continue;
            }
            let patch = self.store.unified_diff(&r.id, &d).await?;
            if patch.starts_with("Binary files") {
                continue;
            }
            let added = patch.lines().skip_while(|l| !l.starts_with("@@")).filter_map(|l| l.strip_prefix('+'));
            for line in added.filter(|l| l.len() <= MAX_LINE) {
                candidates(line, &mut out);
                if out.len() >= MAX_CANDIDATES {
                    break;
                }
            }
        }
        Ok(out.into_iter().take(MAX_CANDIDATES).collect())
    }
}

/// Whether `s` could be a credential rather than ordinary configuration that's also kept as an
/// environment variable (`https://shop.example`, `production`, `staging-database`): at least 12
/// characters, two kinds of character (a digit or a capital among them), 3 bits of entropy a character, and never a URL without a
/// password in it. A low-entropy password slips through; a shared URL never blocks a landing.
pub fn credential_like(s: &str) -> bool {
    let n = s.chars().count();
    if n < CREDENTIAL_MIN_LEN {
        return false;
    }
    if let Some((_, rest)) = s.split_once("://") {
        let userinfo = rest.split(['/', '?', '#']).next().unwrap_or_default().rsplit_once('@').map(|(u, _)| u);
        if !userinfo.is_some_and(|u| u.contains(':')) {
            return false;
        }
    }
    let kinds = [s.chars().any(|c| c.is_ascii_lowercase()), s.chars().any(|c| c.is_ascii_uppercase()), s.chars().any(|c| c.is_ascii_digit()), s.chars().any(|c| !c.is_ascii_alphanumeric())];
    // Hyphenated words (`staging-database`) are configuration: a credential has a digit or a
    // capital in it.
    if kinds.iter().filter(|k| **k).count() < 2 || !(kinds[1] || kinds[2]) {
        return false;
    }
    let mut counts = std::collections::HashMap::new();
    for c in s.chars() {
        *counts.entry(c).or_insert(0usize) += 1;
    }
    let entropy: f64 = counts.values().map(|&k| {
        let p = k as f64 / n as f64;
        -p * p.log2()
    }).sum();
    entropy >= 3.0
}

/// The shortest candidate (roll secrets also never matches values under 8 characters).
const CREDENTIAL_MIN_LEN: usize = 12;

/// A line's candidates: runs of `[A-Za-z0-9_\-+/=.:@~]`, the right-hand side of `KEY=VALUE` and
/// `key: value` (quotes and a trailing `,` or `;` stripped), and the whole trimmed line.
fn candidates(line: &str, out: &mut HashSet<String>) {
    let mut add = |s: &str| {
        if credential_like(s) {
            out.insert(s.to_string());
        }
    };
    let token = |ch: char| ch.is_ascii_alphanumeric() || "_-+/=.:@~".contains(ch);
    for run in line.split(|ch| !token(ch)) {
        add(run);
    }
    let trimmed = line.trim();
    add(trimmed);
    if let Some(i) = trimmed.find(['=', ':']) {
        let rhs = trimmed[i + 1..].trim().trim_end_matches([',', ';']).trim();
        let unquoted = ['"', '\'', '`'].iter().find_map(|q| rhs.strip_prefix(*q).and_then(|s| s.strip_suffix(*q))).unwrap_or(rhs);
        add(unquoted);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn configuration_never_looks_like_a_credential() {
        for plain in ["https://shop.example.com/api", "production", "staging-database", "aaaaaaaaaaaaaaaa", "postgres://db.internal:5432/app"] {
            assert!(!super::credential_like(plain), "{plain}");
        }
        for secret in ["sk_live_51Hx9Q2abc", "postgres://app:s3cr3t-Pw@db.internal/app", "AKIAIOSFODNN7EXAMPLE"] {
            assert!(super::credential_like(secret), "{secret}");
        }
    }
}
