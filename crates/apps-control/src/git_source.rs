//! A public Git URL as a service's source (Render's, Railway's and Cloud Run's "paste your
//! repo"): a GitHub, GitLab or any smart-HTTP repo, with no app installed and no mirror. A deploy
//! resolves the branch's head (`ls-refs`), fetches that one commit shallowly (`code_git::Client`
//! through `net::client`: public addresses only, no redirects, a timeout, its pack bounded by
//! `Limits`), and writes its tree to disk with the hostile-tree rules (`write_tree`). A followed
//! branch deploys its new commits (`deploy::watch_git`), as a followed image tag does.

use code_git::{Client, FetchArgs, Kind, Limits, Object, ObjectId, mode, parse_commit, tree_entries};
use std::collections::HashMap;
use std::path::Path;

/// At most this many files in a fetched tree.
pub const MAX_FILES: usize = 100_000;
/// How deep a fetched tree may nest.
const MAX_DEPTH: usize = 64;

impl crate::Apps {
    /// Every environment of a Git source that this node owns, checked for a new commit on its
    /// branch (`ls-refs`: a few hundred bytes, no fetch). As a watched tag is: a deploy already
    /// on its way is left alone, a paused or held one waits for a person, and each remembers the
    /// commit it last acted on (`tag_seen`), so a rollback isn't undone and a commit whose
    /// deploy failed isn't tried every five minutes. The deployments it started.
    pub async fn watch_git(self: &std::sync::Arc<Self>) -> Vec<String> {
        let mut started = Vec::new();
        let Ok(services) = self.db(|s| s.services(None)).await else { return started };
        for svc in services.into_iter().filter(|s| self.owns(&s.id) && !self.capped(&s.id)) {
            let crate::model::Source::Git { url, .. } = &svc.source else { continue };
            let remote = self.git_remote(&svc.id, url).await;
            let sid = svc.id.clone();
            let Ok((envs, pending)) = self.db(move |s| Ok((s.environments(&sid)?, s.pending_deployments(&sid)?))).await else { continue };
            for env in envs.into_iter().filter(|e| !e.ephemeral && !e.paused && e.held_by.is_none()) {
                // One whose first deploy failed is followed too: the fix pushed next deploys.
                let Some(branch) = env.branch.clone() else { continue };
                if pending.iter().any(|d| d.environment == env.name) {
                    continue;
                }
                let sink = || self.logs.sink(format!("env:{}/{}", svc.id, env.name), crate::logs::Stream::System);
                let sha = match head(&remote, Some(&branch)).await {
                    Ok((sha, _)) => sha,
                    Err(e) => {
                        sink().line(format!("watching {url} ({branch}): {e:#}"));
                        continue;
                    }
                };
                if env.tag_seen.as_deref() == Some(sha.as_str()) {
                    continue;
                }
                let (sid, e, n, a) = (svc.id.clone(), env.name.clone(), sha.clone(), env.active.clone());
                let Ok(live) = self.db(move |s| s.set_tag_seen(&sid, &e, &n).and_then(|_| a.map(|a| s.deployment(&a)).transpose())).await else { continue };
                if live.is_some_and(|l| l.revision.as_deref() == Some(sha.as_str())) {
                    continue;
                }
                // Its message, as a push's hook reads it: `[skip ci]` skips it, its subject names it.
                // One that can't be read isn't deployed blind (its skip tag would be missed): the
                // next pass tries again.
                let message = match commit_message(&remote, &sha).await {
                    Ok(m) => m,
                    Err(e) => {
                        sink().line(format!("{} on {branch}: its message couldn't be read ({e:#}); trying again in five minutes", &sha[..12]));
                        let (sid, e2) = (svc.id.clone(), env.name.clone());
                        let _ = self.db(move |s| s.set_tag_seen(&sid, &e2, "")).await;
                        continue;
                    }
                };
                let file = self.app_config_at(&svc, &sha).await;
                if let Some(why) = self.git_push_skip(&svc, &env, &sha, &message, file.as_ref()).await {
                    sink().line(format!("{} on {branch} not deployed: {why}", &sha[..12]));
                    continue;
                }
                let subject = message.lines().next().unwrap_or("").chars().take(500).collect::<String>();
                let body = serde_json::json!({ "revision": sha, "message": if subject.is_empty() { format!("new commit on {branch}: {}", &sha[..12]) } else { subject } });
                let by = crate::model::By { trigger: crate::model::Trigger::Push, actor: Some("git watch".into()) };
                // Keyed by the commit: two nodes' watches (a takeover mid-pass) make one deploy.
                match self.deploy(&svc.id, &env.name, body, by, None, Some(&format!("git:{}:{sha}", env.name))).await {
                    Ok(d) => started.push(d.id),
                    Err(e) => sink().line(format!("deploying {url} at {}: {e}", &sha[..12])),
                }
            }
        }
        started
    }
}

/// A Git URL a service may name: `https://` (`http://` only where private addresses are allowed,
/// local development), a host and a path, no credentials in it, at most 500 characters.
pub fn check_url(url: &str, allow_private: bool) -> Result<(), String> {
    let bad = |why: &str| Err(format!("source.url: {why}"));
    if url.len() > 500 {
        return bad("at most 500 characters");
    }
    let u = url::Url::parse(url).map_err(|e| format!("source.url: {e}"))?;
    if u.scheme() != "https" && !(allow_private && u.scheme() == "http") {
        return bad("an https:// Git URL");
    }
    if !u.username().is_empty() || u.password().is_some() {
        return bad("a public repository's URL, without credentials in it");
    }
    let Some(host) = u.host_str() else { return bad("no host") };
    if !allow_private && crate::net::literal_private(host) {
        return bad("a private address");
    }
    if u.path().trim_matches('/').is_empty() {
        return bad("the repository's path (https://github.com/owner/name)");
    }
    Ok(())
}

/// The client a deploy fetches with.
fn client(allow_private: bool) -> Client {
    Client::with_http(crate::net::client(allow_private, std::time::Duration::from_secs(120)), Limits::default())
}

/// A repository to fetch from: the client, its URL, and a private repository's login (sent as
/// Basic auth, only ever to this URL: the client follows no redirect). No `Debug`: it holds the
/// password.
pub struct Remote<'a> {
    client: Client,
    pub url: &'a str,
    auth: Option<code_git::Credentials>,
}

impl<'a> Remote<'a> {
    pub fn new(allow_private: bool, url: &'a str, login: Option<(String, String)>) -> Self {
        Remote { client: client(allow_private), url, auth: login.map(|(username, password)| code_git::Credentials { username, password, vouched: false }) }
    }

    /// A public repository's (a plan, an import, a template): no login.
    pub fn public(allow_private: bool, url: &'a str) -> Self {
        Self::new(allow_private, url, None)
    }

    pub async fn ls_refs(&self) -> anyhow::Result<code_git::RefList> {
        self.client.ls_refs(self.url, self.auth.as_ref()).await
    }

    async fn fetch(&self, args: &FetchArgs) -> anyhow::Result<code_git::Fetched> {
        self.client.fetch(self.url, self.auth.as_ref(), args).await
    }
}

/// A branch's head: its commit and its name (the repository's default when none is named).
pub async fn head(r: &Remote<'_>, branch: Option<&str>) -> anyhow::Result<(String, String)> {
    let url = r.url;
    let refs = r.ls_refs().await?;
    let name = match branch {
        Some(b) => format!("refs/heads/{b}"),
        None => refs.head.clone().ok_or_else(|| anyhow::anyhow!("{url} has no default branch"))?,
    };
    let r = refs.get(&name).ok_or_else(|| anyhow::anyhow!("{url} has no branch {}", name.trim_start_matches("refs/heads/")))?;
    Ok((r.oid.to_string(), name.trim_start_matches("refs/heads/").to_string()))
}

/// The tree at `sha`, fetched shallowly and written into `dir` (empty). How many files.
pub async fn materialize(r: &Remote<'_>, sha: &str, dir: &Path) -> anyhow::Result<usize> {
    let oid: ObjectId = sha.parse().map_err(|_| anyhow::anyhow!("{sha} isn't a commit id"))?;
    let fetched = r.fetch(&FetchArgs { wants: vec![oid], haves: Vec::new(), depth: Some(1), filter: None }).await?;
    let objects: HashMap<ObjectId, Object> = fetched.objects.into_iter().map(|o| (o.id, o)).collect();
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || write_commit(&objects, oid, &dir)).await?
}

/// A fetch that leaves objects out with a filter, or, from a server that takes no filters (some
/// self-hosted ones), the same fetch with everything (still at most `Limits::max_pack_bytes`).
async fn fetch_filtered(r: &Remote<'_>, args: FetchArgs) -> anyhow::Result<code_git::Fetched> {
    match r.fetch(&args).await {
        Err(e) if args.filter.is_some() && e.to_string().contains("does not support filters") => r.fetch(&FetchArgs { filter: None, ..args }).await,
        r => r,
    }
}

/// The paths that differ between the trees of commits `from` and `to`: one shallow fetch of
/// both with no blob (trees compared by id, so an unchanged folder is never walked), at most
/// 10,000 paths (past that, everything counts as changed: `None`).
pub async fn changed_paths(r: &Remote<'_>, from: &str, to: &str) -> anyhow::Result<Option<Vec<String>>> {
    let (a, b): (ObjectId, ObjectId) = (from.parse().map_err(|_| anyhow::anyhow!("{from} isn't a commit id"))?, to.parse().map_err(|_| anyhow::anyhow!("{to} isn't a commit id"))?);
    let args = FetchArgs { wants: vec![a, b], haves: Vec::new(), depth: Some(1), filter: Some(code_git::Filter::BlobNone) };
    let objects: HashMap<ObjectId, Object> = fetch_filtered(r, args).await?.objects.into_iter().map(|o| (o.id, o)).collect();
    let tree_of = |c: ObjectId| -> anyhow::Result<ObjectId> {
        let o = objects.get(&c).filter(|o| o.kind == Kind::Commit).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring commit {c}"))?;
        Ok(parse_commit(&o.data).map_err(|e| anyhow::anyhow!("{e}"))?.tree)
    };
    let mut out = Vec::new();
    let ok = diff_trees(&objects, Some(tree_of(a)?), Some(tree_of(b)?), "", &mut out, 0)?;
    Ok(ok.then_some(out))
}

/// The paths under `prefix` that differ between two trees (either may be absent); false once
/// past the cap.
fn diff_trees(objects: &HashMap<ObjectId, Object>, a: Option<ObjectId>, b: Option<ObjectId>, prefix: &str, out: &mut Vec<String>, depth: usize) -> anyhow::Result<bool> {
    if a == b {
        return Ok(true);
    }
    // Deeper than a tree is written to disk: everything counts as changed.
    if depth >= MAX_DEPTH {
        return Ok(false);
    }
    let entries = |t: Option<ObjectId>| -> anyhow::Result<HashMap<String, (u32, ObjectId)>> {
        let Some(t) = t else { return Ok(HashMap::new()) };
        let o = objects.get(&t).filter(|o| o.kind == Kind::Tree).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring tree {t}"))?;
        tree_entries(&o.data).map(|e| e.map(|e| (String::from_utf8_lossy(e.name).into_owned(), (e.mode, e.oid))).map_err(|e| anyhow::anyhow!("{e}"))).collect()
    };
    let (ea, eb) = (entries(a)?, entries(b)?);
    let mut names: Vec<&String> = ea.keys().chain(eb.keys()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let (x, y) = (ea.get(name), eb.get(name));
        if x == y {
            continue;
        }
        let path = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
        let tree = |e: Option<&(u32, ObjectId)>| e.filter(|(m, _)| *m == mode::TREE).map(|(_, id)| *id);
        if (tree(x).is_some() || tree(y).is_some()) && !diff_trees(objects, tree(x), tree(y), &path, out, depth + 1)? {
            return Ok(false);
        }
        if tree(x).is_none() && x.is_some() || tree(y).is_none() && y.is_some() {
            out.push(path);
            if out.len() > 10_000 {
                return Ok(false);
            }
        }
    }
    Ok(true)
}

/// A commit's message: one shallow fetch of the commit alone (no tree, no blob).
pub async fn commit_message(r: &Remote<'_>, sha: &str) -> anyhow::Result<String> {
    let oid: ObjectId = sha.parse().map_err(|_| anyhow::anyhow!("{sha} isn't a commit id"))?;
    let args = FetchArgs { wants: vec![oid], haves: Vec::new(), depth: Some(1), filter: Some(code_git::Filter::TreeNone) };
    let fetched = fetch_filtered(r, args).await?;
    let c = fetched.objects.into_iter().find(|o| o.id == oid && o.kind == Kind::Commit).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring commit {oid}"))?;
    let text = String::from_utf8_lossy(&c.data).into_owned();
    Ok(text.split_once("\n\n").map(|(_, m)| m.to_string()).unwrap_or_default())
}

/// The file at `path` (`roll.toml`, or a root's `web/roll.toml`) in the tree at `sha`, at most
/// 64 KiB: one shallow fetch that leaves out every bigger blob. None: no such file.
pub async fn read_file(r: &Remote<'_>, sha: &str, path: &str) -> anyhow::Result<Option<Vec<u8>>> {
    const MAX: u64 = 64 << 10;
    let oid: ObjectId = sha.parse().map_err(|_| anyhow::anyhow!("{sha} isn't a commit id"))?;
    let args = FetchArgs { wants: vec![oid], haves: Vec::new(), depth: Some(1), filter: Some(code_git::Filter::BlobLimit(MAX)) };
    let objects: HashMap<ObjectId, Object> = fetch_filtered(r, args).await?.objects.into_iter().map(|o| (o.id, o)).collect();
    let c = objects.get(&oid).filter(|o| o.kind == Kind::Commit).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring commit {oid}"))?;
    let mut tree = parse_commit(&c.data).map_err(|e| anyhow::anyhow!("{e}"))?.tree;
    let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let Some((name, folders)) = segments.split_last() else { return Ok(None) };
    // Down the folders (trees only, never a symlink), then the file in the last one.
    for (i, want) in segments.iter().enumerate() {
        let t = objects.get(&tree).filter(|o| o.kind == Kind::Tree).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring tree {tree}"))?;
        let mut found = None;
        for e in tree_entries(&t.data) {
            let e = e.map_err(|e| anyhow::anyhow!("{e}"))?;
            if e.name == want.as_bytes() {
                found = Some((e.mode, e.oid));
                break;
            }
        }
        match found {
            Some((mode::TREE, oid)) if i < folders.len() => tree = oid,
            Some((mode::REGULAR | mode::EXECUTABLE, oid)) if i == folders.len() => {
                return match objects.get(&oid).filter(|o| o.kind == Kind::Blob) {
                    Some(b) => Ok(Some(b.data.to_vec())),
                    None => anyhow::bail!("its {name} is over 64 KiB"),
                };
            }
            _ => return Ok(None),
        }
    }
    Ok(None)
}

/// A commit's tree written into `dir`.
pub fn write_commit(objects: &HashMap<ObjectId, Object>, commit: ObjectId, dir: &Path) -> anyhow::Result<usize> {
    let c = objects.get(&commit).filter(|o| o.kind == Kind::Commit).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring commit {commit}"))?;
    let info = parse_commit(&c.data).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut files = 0;
    write_tree(objects, info.tree, dir, 0, &mut files)?;
    Ok(files)
}

/// A tree written with the hostile-tree rules: a name is one path segment (never `.`, `..`,
/// `.git` or one with a `/`), nothing is written over (a repeated name fails), symlinks and
/// submodules aren't followed (left out: a link out of the tree would build from the host),
/// and the files and depth are bounded.
fn write_tree(objects: &HashMap<ObjectId, Object>, tree: ObjectId, dir: &Path, depth: usize, files: &mut usize) -> anyhow::Result<()> {
    anyhow::ensure!(depth < MAX_DEPTH, "the tree nests deeper than {MAX_DEPTH}");
    let t = objects.get(&tree).filter(|o| o.kind == Kind::Tree).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring tree {tree}"))?;
    for e in tree_entries(&t.data) {
        let e = e.map_err(|e| anyhow::anyhow!("{e}"))?;
        let name = std::str::from_utf8(e.name).map_err(|_| anyhow::anyhow!("a name in the tree isn't UTF-8"))?;
        anyhow::ensure!(!name.is_empty() && !name.contains(['/', '\0']) && name != "." && name != ".." && !name.eq_ignore_ascii_case(".git"), "the tree names {name:?}");
        let path = dir.join(name);
        match e.mode {
            mode::TREE => {
                std::fs::create_dir(&path).map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
                write_tree(objects, e.oid, &path, depth + 1, files)?;
            }
            mode::REGULAR | mode::EXECUTABLE => {
                let blob = objects.get(&e.oid).filter(|o| o.kind == Kind::Blob).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring {name}"))?;
                *files += 1;
                anyhow::ensure!(*files <= MAX_FILES, "more than {MAX_FILES} files");
                use std::io::Write;
                use std::os::unix::fs::OpenOptionsExt;
                let mut f = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(if e.mode == mode::EXECUTABLE { 0o755 } else { 0o644 })
                    .open(&path)
                    .map_err(|err| anyhow::anyhow!("{name}: {err}"))?;
                f.write_all(&blob.data)?;
            }
            // Symlinks and submodules: not followed.
            _ => {}
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(entries: &[(u32, &str, ObjectId)]) -> Object {
        let mut data = Vec::new();
        for (m, n, id) in entries {
            data.extend_from_slice(format!("{m:o} {n}\0").as_bytes());
            data.extend_from_slice(&id.0);
        }
        Object::new(Kind::Tree, data)
    }

    #[test]
    fn a_fetched_tree_is_written_by_the_hostile_tree_rules() {
        let blob = Object::new(Kind::Blob, b"console.log(1)".to_vec());
        let sub = tree(&[(mode::REGULAR, "app.js", blob.id)]);
        let root = tree(&[(mode::TREE, "src", sub.id), (mode::EXECUTABLE, "start.sh", blob.id), (mode::SYMLINK, "escape", blob.id)]);
        let commit = Object::new(Kind::Commit, format!("tree {}\n\nfirst\n", root.id).into_bytes());
        let objects: HashMap<_, _> = [blob, sub, root, commit.clone()].into_iter().map(|o| (o.id, o)).collect();
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(write_commit(&objects, commit.id, dir.path()).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(dir.path().join("src/app.js")).unwrap(), "console.log(1)");
        assert!(std::fs::symlink_metadata(dir.path().join("escape")).is_err(), "a symlink isn't followed or made");
        // A repeated name, or one that climbs out, is refused.
        for bad in [vec![(mode::REGULAR, "a", objects.values().find(|o| o.kind == Kind::Blob).unwrap().id), (mode::REGULAR, "a", objects.values().find(|o| o.kind == Kind::Blob).unwrap().id)], vec![(mode::REGULAR, "..", objects.values().find(|o| o.kind == Kind::Blob).unwrap().id)]] {
            let t = tree(&bad);
            let c = Object::new(Kind::Commit, format!("tree {}\n\nx\n", t.id).into_bytes());
            let mut o2 = objects.clone();
            o2.insert(t.id, t);
            o2.insert(c.id, c.clone());
            assert!(write_commit(&o2, c.id, tempfile::tempdir().unwrap().path()).is_err());
        }
    }

    #[test]
    fn only_public_https_urls() {
        assert!(check_url("https://github.com/acme/site", false).is_ok());
        assert!(check_url("http://github.com/acme/site", false).is_err());
        assert!(check_url("https://user:token@github.com/acme/site", false).is_err());
        assert!(check_url("https://127.0.0.1/acme/site", false).is_err());
        assert!(check_url("https://github.com/", false).is_err());
    }
}
