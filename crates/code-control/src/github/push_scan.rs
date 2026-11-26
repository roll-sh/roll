//! Pushes to a connected GitHub repo checked for roll secrets' values (docs/secrets.md, "On
//! GitHub.com: roll code's GitHub App"). GitHub.com runs no pre-receive hook, so nothing blocks
//! the push: after it, its added lines are matched as roll secrets' scanner matches them, a value
//! found is a leak (`secret.leaked`, rotated under the project's `leaks.rotate`), and the commit
//! gets a `roll secrets` check run that fails, which branch protection can require.
//!
//! - **Queued, not run, by the push.** [`queue`] adds a `push_scan` delivery to the one queue
//!   (`review_deliveries`, target `gh_<repo>`) once the push's commits are in roll, so the scan
//!   and its check run happen on the repo's owner, retried until GitHub answers.
//! - **What it reads.** The commits the push brought (reachable from `after`, not from `before`
//!   or, for a branch other than the default, the default branch), each one's lines new against
//!   every parent; and for such a branch, everything it adds since its merge base, so the check
//!   stays failing while the branch still adds a value (a later clean push doesn't hide it).
//! - **Bounded as the pre-receive hook is**: [`COMMITS`] commits, [`BYTES`] read, files up to
//!   [`FILE`], lines up to [`LINE`], [`CANDIDATES`] candidates (one call of roll secrets' scan).
//!   What isn't read is named in the check run, and the check fails: never passed unread.
//! - **Only the repo's own pushes**: a branch of the installed repo itself. A fork's pull request
//!   is GitHub's `pull_request`, never a push here; tags and roll's own mirror pushes (landings,
//!   which roll's blockers checked) aren't read.
//! - **Never a value**: the check names `file:line KEY (environment)`; candidates go to roll
//!   secrets in process and nowhere else.

use super::outbox::{self, Job};
use super::{Hub, link_of, mark, set_mark};
use crate::model::RepoRow;
use crate::{Code, Error, Result};
use code_github::PushDraft;
use code_github::model::{CheckConclusion, CheckOutput, CheckRunRequest, CheckStatus as GhStatus};
use code_store::{ChangesetId, Mode, Oid};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};

/// The check run's name.
pub(crate) const CHECK: &str = "roll secrets";
/// The most commits one push's scan reads.
const COMMITS: usize = 1000;
/// The most bytes it reads, the largest file and the longest line (the pre-receive hook's).
const BYTES: u64 = 256 << 20;
const FILE: u64 = 64 << 20;
const LINE: usize = 8 << 20;
/// The most candidates it asks about (one scan of roll secrets: `SCAN_MAX`), and keeps.
const CANDIDATES: usize = 20_000;
const CANDIDATE_BYTES: usize = 32 << 20;
/// Places kept per candidate, values listed in the check, and what wasn't read.
const PLACES: usize = 20;
const SHOWN: usize = 50;
const UNREAD_SHOWN: usize = 20;

/// After a push's commits are in roll (its handler did its work): its scan, when the repo is on a
/// roll secrets project. A deletion, a tag, or a push roll secrets doesn't scan queues nothing.
pub(super) async fn queue(code: &Code, r: &RepoRow, p: &PushDraft) -> Result<()> {
    let (Some(_), Some(after)) = (p.branch(), &p.after) else { return Ok(()) };
    let Some(secrets) = code.secret_env.get().cloned() else { return Ok(()) };
    // When roll secrets can't say, the scan asks again.
    if !secrets.scans_pushes(&r.full_name()).await.unwrap_or(true) {
        return Ok(());
    }
    let job = Job::PushScan { r#ref: p.ref_name.clone(), before: p.before.clone(), after: after.clone(), pusher: p.sender.clone() };
    code.db.write(|c| outbox::queue_job(c, &r.id, &job))?;
    code.kick_deliveries();
    Ok(())
}

/// A finished scan, kept per commit (`secret-scan-out:`) so a retry of its check run doesn't
/// read and ask again.
#[derive(Serialize, Deserialize)]
struct Outcome {
    failed: bool,
    title: String,
    summary: String,
}

/// The `push_scan` delivery, on the repo's owner: scan the push (once), then its check run.
pub(super) async fn run(code: &Code, hub: &Hub, repo_id: &str, r#ref: &str, before: Option<&str>, after: &str, pusher: Option<&str>) -> Result<()> {
    let Some(secrets) = code.secret_env.get().cloned() else { return Ok(()) };
    let Some(r) = code.db.repo_by_id(repo_id)? else { return Ok(()) };
    let Some(link) = link_of(&r).filter(|l| l.connected) else { return Ok(()) };
    let out_key = format!("secret-scan-out:{repo_id}:{after}");
    let outcome = match mark(code, &out_key)?.and_then(|v| serde_json::from_str::<Outcome>(&v).ok()) {
        Some(o) => o,
        None => {
            if !secrets.scans_pushes(&r.full_name()).await.map_err(Error::Internal)? {
                return Ok(());
            }
            let Some(head) = oid(after).and_then(|o| code.store.changeset_of_git(repo_id, &o).ok().flatten()) else {
                // The push's handler brought it in before queueing this: it's gone since (the repo
                // was imported again), and a newer push has its own scan.
                tracing::warn!(repo = %r.full_name(), commit = %after, "a pushed commit to scan isn't in roll");
                return Ok(());
            };
            let read = read(code, &r, r#ref, before, head).await?;
            let push = crate::PushScan { installation: link.installation_id, pusher: pusher.map(str::to_string), r#ref: r#ref.chars().take(256).collect() };
            let o = ask(&*secrets, &r.full_name(), &push, read).await?;
            set_mark(code, &out_key, &serde_json::to_string(&o).unwrap_or_default())?;
            o
        }
    };
    let conclusion = if outcome.failed { CheckConclusion::Failure } else { CheckConclusion::Success };
    let output = Some(CheckOutput { title: outcome.title, summary: outcome.summary, text: None });
    let gh = |e: code_github::Error| Error::Internal(e.into());
    let run_key = format!("secret-scan:{repo_id}:{after}");
    match mark(code, &run_key)?.and_then(|v| v.parse::<u64>().ok()) {
        Some(id) => {
            let req = CheckRunRequest { status: Some(GhStatus::Completed), conclusion: Some(conclusion), output, ..Default::default() };
            hub.client.update_check_run(link.installation_id, &link.owner, &link.repo, id, &req).await.map_err(gh)?;
        }
        None => {
            let req = CheckRunRequest {
                name: Some(CHECK.into()),
                head_sha: Some(after.to_string()),
                status: Some(GhStatus::Completed),
                conclusion: Some(conclusion),
                details_url: Some(code.web_url(&r)),
                output,
                ..Default::default()
            };
            let run = hub.client.create_check_run(link.installation_id, &link.owner, &link.repo, &req).await.map_err(gh)?;
            set_mark(code, &run_key, &run.id.to_string())?;
        }
    }
    Ok(())
}

fn oid(hex: &str) -> Option<Oid> {
    code_git::ObjectId::from_hex(hex.as_bytes()).map(|o| Oid(o.0))
}

/// Where a candidate is: a file's line, in a commit (its short id) or in the branch since its
/// merge base.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Place {
    file: String,
    line: usize,
    commit: Option<String>,
}

/// What a push's scan read: each candidate once with its first places, how many commits, and
/// what it didn't read.
#[derive(Default)]
struct Read {
    index: HashMap<String, usize>,
    places: Vec<Vec<Place>>,
    commits: usize,
    branch: bool,
    bytes: u64,
    kept: usize,
    unread: Vec<String>,
    /// A budget ran out: nothing more is read.
    full: bool,
}

impl Read {
    fn add(&mut self, c: String, at: &Place) -> bool {
        let n = self.index.len();
        let len = c.len();
        let i = match self.index.get(&c) {
            Some(i) => *i,
            None if n >= CANDIDATES || self.kept + len > CANDIDATE_BYTES => return false,
            None => {
                self.index.insert(c, n);
                self.kept += len;
                self.places.push(Vec::new());
                n
            }
        };
        if self.places[i].len() < PLACES {
            self.places[i].push(at.clone());
        }
        true
    }

    fn unread(&mut self, what: String) {
        self.unread.push(what);
    }

    /// The lines of blob `new` at `file` that no blob of `old` has (its parents' versions), cut
    /// into candidates.
    async fn file(&mut self, code: &Code, repo: &str, file: &str, new: Oid, old: &[Oid], commit: Option<&str>) -> Result<()> {
        if self.full {
            return Ok(());
        }
        let place = |what: &str| match commit {
            Some(c) => format!("{file} in {c}: {what}"),
            None => format!("{file}: {what}"),
        };
        let size = |o: &Oid| code.store.blob_size(repo, o).ok().flatten().unwrap_or(u64::MAX);
        let n = size(&new);
        if n > FILE {
            self.unread(place("over 64 MiB"));
            return Ok(());
        }
        // Old versions too large to hold count as empty: their lines are read as added.
        let old: Vec<Oid> = old.iter().copied().filter(|o| size(o) <= FILE).collect();
        self.bytes += n + old.iter().map(size).sum::<u64>();
        if self.bytes > BYTES {
            self.full = true;
            self.unread(place("and everything after it: past 256 MiB read"));
            return Ok(());
        }
        let Some(data) = code.store.get_blob(repo, &new).await? else { return Ok(()) };
        if data.contains(&0) {
            return Ok(());
        }
        let mut olds = Vec::new();
        for o in &old {
            if let Some(b) = code.store.get_blob(repo, o).await? {
                olds.push(b);
            }
        }
        let had: HashSet<&[u8]> = olds.iter().flat_map(|b| b.split(|c| *c == b'\n')).collect();
        for (i, line) in data.split(|c| *c == b'\n').enumerate() {
            if line.len() > LINE {
                self.unread(place(&format!("line {} is over 8 MiB", i + 1)));
                continue;
            }
            if had.contains(line) {
                continue;
            }
            let mut found = HashSet::new();
            crate::secret_leaks::candidates(&String::from_utf8_lossy(line), &mut found);
            let at = Place { file: file.to_string(), line: i + 1, commit: commit.map(str::to_string) };
            for c in found {
                if !self.add(c, &at) {
                    self.full = true;
                    self.unread(place(&format!("line {} and everything after it: past {CANDIDATES} candidates", i + 1)));
                    return Ok(());
                }
            }
        }
        Ok(())
    }
}

/// Read what the push to `r#ref` (now at `head`) adds (see the module doc).
async fn read(code: &Code, r: &RepoRow, r#ref: &str, before: Option<&str>, head: ChangesetId) -> Result<Read> {
    let repo = r.id.as_str();
    let default = r#ref == code_store::full_ref(&r.default_branch);
    let before = before.and_then(oid).and_then(|o| code.store.changeset_of_git(repo, &o).ok().flatten());
    let main = if default { None } else { code.head(r)? };
    let stops: Vec<ChangesetId> = before.into_iter().chain(main).collect();
    let mut read = Read::default();
    // The branch since its merge base first: what the check is about.
    let mb = if default { None } else { code.merge_base(r, &head)? };
    if let Some(mb) = mb.filter(|mb| *mb != head) {
        read.branch = true;
        for d in code.store.compare(repo, &mb, &head)? {
            if let (Some(new), Some(mode)) = (d.new_oid, d.new_mode)
                && mode != Mode::Gitlink
            {
                read.file(code, repo, d.path.as_str(), new, &d.old_oid.into_iter().collect::<Vec<_>>(), None).await?;
            }
        }
    }
    let (store, id) = (code.store.clone(), repo.to_string());
    let (commits, more) = tokio::task::spawn_blocking(move || new_commits(&store, &id, head, &stops)).await.map_err(|e| Error::Internal(e.into()))??;
    if more {
        read.unread(format!("commits past the newest {COMMITS} this push brought"));
    }
    for cs in commits {
        if read.full {
            break;
        }
        read.commits += 1;
        let Some(meta) = code.store.meta(repo, &cs)? else { continue };
        let short = code.store.derive_git(repo, &cs)?.0.hex()[..7].to_string();
        let others: Vec<ChangesetId> = meta.cs.parents.iter().skip(1).copied().collect();
        'files: for d in code.store.diff(repo, &cs)? {
            let (Some(new), Some(mode)) = (d.new_oid, d.new_mode) else { continue };
            if mode == Mode::Gitlink {
                continue;
            }
            // A merge's lines count when they're new against every parent: a side's were read in
            // that side's commits.
            let mut old: Vec<Oid> = d.old_oid.into_iter().collect();
            for p in &others {
                match code.store.entry(repo, p, d.path.as_str())? {
                    Some(e) if e.oid == new => continue 'files,
                    Some(e) if e.file_mode().is_some() => old.push(e.oid),
                    _ => {}
                }
            }
            read.file(code, repo, d.path.as_str(), new, &old, Some(&short)).await?;
        }
    }
    Ok(read)
}

/// The commits reachable from `head` and from none of `stops`, newest first: at most
/// [`COMMITS`] (true when there were more).
fn new_commits(store: &code_store::Store, repo: &str, head: ChangesetId, stops: &[ChangesetId]) -> anyhow::Result<(Vec<ChangesetId>, bool)> {
    let (mut seen, mut queue, mut out) = (HashSet::new(), VecDeque::from([head]), Vec::new());
    while let Some(c) = queue.pop_front() {
        if !seen.insert(c) {
            continue;
        }
        let mut known = false;
        for s in stops {
            if store.is_ancestor(repo, &c, s)? {
                known = true;
                break;
            }
        }
        if known {
            continue;
        }
        if out.len() >= COMMITS {
            return Ok((out, true));
        }
        out.push(c);
        if let Some(m) = store.meta(repo, &c)? {
            queue.extend(m.cs.parents.iter().copied());
        }
    }
    Ok((out, false))
}

/// Ask roll secrets about what was read and say what it found: where, the key and the
/// environment, never the value.
async fn ask(secrets: &dyn crate::SecretEnv, repo: &str, push: &crate::PushScan, mut read: Read) -> Result<Outcome> {
    let mut candidates = vec![String::new(); read.index.len()];
    for (c, i) in read.index.drain() {
        candidates[i] = c;
    }
    let found = if candidates.is_empty() { Vec::new() } else { secrets.scan_push(repo, push, candidates).await.map_err(Error::Internal)? };
    let mut hits: BTreeSet<(Place, String, String)> = BTreeSet::new();
    for l in &found {
        for p in read.places.get(l.index).into_iter().flatten() {
            hits.insert((p.clone(), l.key.clone(), l.environment.clone()));
        }
    }
    let lines: Vec<String> = hits
        .iter()
        .map(|(p, key, env)| {
            let at = p.commit.as_deref().map(|c| format!(" in {c}")).unwrap_or_default();
            format!("- `{}:{}` {key} ({env}){at}", p.file, p.line)
        })
        .collect();
    let mut list = lines.iter().take(SHOWN).cloned().collect::<Vec<_>>().join("\n");
    if lines.len() > SHOWN {
        list.push_str(&format!("\n- and {} more", lines.len() - SHOWN));
    }
    let mut unread = read.unread.iter().take(UNREAD_SHOWN).map(|u| format!("- {u}")).collect::<Vec<_>>().join("\n");
    if read.unread.len() > UNREAD_SHOWN {
        unread.push_str(&format!("\n- and {} more", read.unread.len() - UNREAD_SHOWN));
    }
    let what = match (read.commits, read.branch) {
        (n, true) => format!("{n} new commit{} and the branch since it left the default branch", if n == 1 { "" } else { "s" }),
        (n, false) => format!("{n} new commit{}", if n == 1 { "" } else { "s" }),
    };
    Ok(if !lines.is_empty() {
        let keys: BTreeSet<&str> = hits.iter().map(|(_, k, _)| k.as_str()).collect();
        let mut summary = format!(
            "This push adds {} roll secrets holds. It's on GitHub now: roll secrets has it as leaked (`secret.leaked`), and rotates it where the project rotates leaked keys (`leaks.rotate`). Rotate what it didn't, remove it from the branch and read it from the environment.\n\n{list}",
            if keys.len() == 1 { "a value".to_string() } else { format!("{} values", keys.len()) }
        );
        if !unread.is_empty() {
            summary.push_str(&format!("\n\nNot read:\n{unread}"));
        }
        let title = if keys.len() == 1 { format!("Adds the value of {}", keys.iter().next().copied().unwrap_or_default()) } else { format!("Adds the values of {} keys", keys.len()) };
        Outcome { failed: true, title, summary }
    } else if !unread.is_empty() {
        Outcome {
            failed: true,
            title: "Couldn't read all of this push".into(),
            summary: format!("Read {what}: none of it adds a value roll secrets holds, but this wasn't read, so the check doesn't pass it:\n{unread}"),
        }
    } else {
        Outcome { failed: false, title: "Adds no value of roll secrets".into(), summary: format!("Read {what}: none of it adds a value roll secrets holds.") }
    })
}
