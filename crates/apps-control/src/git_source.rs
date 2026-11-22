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
            for env in envs.into_iter().filter(|e| !e.ephemeral && !e.paused && !matches!(crate::deploy::hold_of(&svc, e), crate::deploy::Held::Skip(_))) {
                let hold = crate::deploy::hold_of(&svc, &env);
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
                if env.wait_for_ci {
                    let _ = self.wait_for_git_ci(&svc, &env.name, &sha, &subject, "git watch").await;
                    continue;
                }
                let body = hold.body(serde_json::json!({ "revision": sha, "message": if subject.is_empty() { format!("new commit on {branch}: {}", &sha[..12]) } else { subject } }));
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
    allow_private: bool,
}

impl<'a> Remote<'a> {
    pub fn new(allow_private: bool, url: &'a str, login: Option<(String, String)>) -> Self {
        Remote { client: client(allow_private), url, auth: login.map(|(username, password)| code_git::Credentials { username, password, vouched: false }), allow_private }
    }

    /// The login it fetches with (a username and a token), if it has one.
    pub fn login(&self) -> Option<(&str, &str)> {
        self.auth.as_ref().map(|c| (c.username.as_str(), c.password.as_str()))
    }

    /// A public repository's (a plan, an import, a template): no login.
    pub fn public(allow_private: bool, url: &'a str) -> Self {
        Self::new(allow_private, url, None)
    }

    pub async fn ls_refs(&self) -> anyhow::Result<code_git::RefList> {
        self.client.ls_refs(self.url, self.auth.as_ref()).await
    }

    /// Where one ref is now (`refs/pull/7/head`), none when the repository hasn't it.
    pub async fn ref_at(&self, name: &str) -> anyhow::Result<Option<String>> {
        let refs = self.client.ls_refs_with(self.url, self.auth.as_ref(), &[name]).await?;
        Ok(refs.get(name).map(|r| r.oid.to_string()))
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

/// A branch's or a tag's commit by its name (`feature-x`, `v1.2.0`), the branch first, a tag
/// peeled to its commit; and which it was. A deploy of one revision, not a branch to follow.
pub async fn named(r: &Remote<'_>, name: &str) -> anyhow::Result<(String, &'static str)> {
    let ok = !name.is_empty() && name.len() <= 255 && !name.starts_with('-') && !name.contains("..") && name.bytes().all(|b| b.is_ascii_graphic() && !b"~^:?*[\\".contains(&b));
    anyhow::ensure!(ok, "{name:?} isn't a branch or tag name");
    let (branch, tag) = (format!("refs/heads/{name}"), format!("refs/tags/{name}"));
    let refs = r.client.ls_refs_with(r.url, r.auth.as_ref(), &[branch.as_str(), tag.as_str()]).await?;
    if let Some(b) = refs.get(&branch) {
        return Ok((b.oid.to_string(), "branch"));
    }
    if let Some(t) = refs.get(&tag) {
        return Ok((t.peeled.unwrap_or(t.oid).to_string(), "tag"));
    }
    anyhow::bail!("{} has no branch or tag {name}", r.url)
}

/// The tree at `sha`, fetched shallowly and written into `dir` (empty). How many files.
pub async fn materialize(r: &Remote<'_>, sha: &str, dir: &Path) -> anyhow::Result<usize> {
    materialize_with(r, sha, dir, false).await
}

/// With `submodules` (`build.submodules`): each submodule `.gitmodules` names is fetched at the
/// commit its gitlink pins and written at its path, from its own URL (public-only, as a service's
/// own; relative URLs against this repository's), with this repository's login only on the same
/// host, two levels deep at most and [`MAX_SUBMODULES`] in all.
pub async fn materialize_with(r: &Remote<'_>, sha: &str, dir: &Path, submodules: bool) -> anyhow::Result<usize> {
    let mut budget = MAX_SUBMODULES;
    materialize_at(r, sha, dir, submodules.then_some(2), &mut budget).await
}

/// A submodule's URL as `.gitmodules` has it, against its superproject's: relative ones as git
/// reads them (the superproject's URL as a directory: `../lib.git` beside it).
pub fn submodule_url(superproject: &str, raw: &str) -> anyhow::Result<url::Url> {
    let base = url::Url::parse(&format!("{}/", superproject.trim_end_matches('/')))?;
    Ok(base.join(raw.trim())?)
}

/// `materialize_at` for a submodule outside roll code under a roll code repo's tree (`levels`
/// more below it), sharing the tree's budget.
pub(crate) async fn materialize_sub(r: &Remote<'_>, sha: &str, dir: &Path, levels: u8, budget: &mut usize) -> anyhow::Result<usize> {
    materialize_at(r, sha, dir, Some(levels), budget).await
}

/// The most submodules one tree brings in, every level counted.
pub const MAX_SUBMODULES: usize = 32;
/// What a tree's Git LFS objects add up to at most, and one at most.
pub const MAX_LFS_BYTES: u64 = 2 << 30;
pub const MAX_LFS_OBJECT: u64 = 1 << 30;
/// An LFS pointer's first line, and the most a pointer file is.
const LFS_POINTER: &str = "version https://git-lfs.github.com/spec/v1";
const LFS_POINTER_MAX: u64 = 1024;

/// A written tree's Git LFS pointers replaced by their objects (`build.lfs`; Vercel's, Netlify's):
/// asked of the repository's LFS batch API (`<url>/info/lfs/objects/batch`, with its login),
/// each downloaded from the address it answers through the public-only client (with only the
/// headers it gives), checked against the pointer's sha256 and size as it's streamed, and written
/// in the pointer's place. [`MAX_LFS_BYTES`] in all, [`MAX_LFS_OBJECT`] each. How many.
pub async fn fetch_lfs(r: &Remote<'_>, dir: &Path) -> anyhow::Result<usize> {
    let d = dir.to_path_buf();
    let pointers = tokio::task::spawn_blocking(move || lfs_pointers(&d)).await??;
    if pointers.is_empty() {
        return Ok(0);
    }
    let total: u64 = pointers.iter().map(|(_, _, size)| size).sum();
    anyhow::ensure!(total <= MAX_LFS_BYTES, "its LFS objects are {total} bytes, more than {MAX_LFS_BYTES}");
    let http = crate::net::client(r.allow_private, std::time::Duration::from_secs(600));
    let batch_url = format!("{}/info/lfs/objects/batch", r.url.trim_end_matches('/'));
    let mut done = 0;
    for chunk in pointers.chunks(100) {
        let objects: Vec<serde_json::Value> = chunk.iter().map(|(_, oid, size)| serde_json::json!({ "oid": oid, "size": size })).collect();
        let mut req = http.post(&batch_url).header("accept", "application/vnd.git-lfs+json").header("content-type", "application/vnd.git-lfs+json").json(&serde_json::json!({ "operation": "download", "transfers": ["basic"], "objects": objects }));
        if let Some(c) = &r.auth {
            req = req.basic_auth(&c.username, Some(&c.password));
        }
        let res = req.send().await.map_err(|e| anyhow::anyhow!("LFS batch: {}", e.without_url()))?;
        anyhow::ensure!(res.status().is_success(), "LFS batch answered {}", res.status());
        let answer: serde_json::Value = serde_json::from_slice(&crate::net::read_bounded(res, 4 << 20).await)?;
        for (path, oid, size) in chunk {
            let o = answer["objects"].as_array().and_then(|a| a.iter().find(|o| o["oid"] == *oid)).ok_or_else(|| anyhow::anyhow!("LFS batch didn't answer {oid}"))?;
            if let Some(e) = o.get("error") {
                anyhow::bail!("LFS object {oid}: {}", e["message"].as_str().unwrap_or("refused"));
            }
            let href = o.pointer("/actions/download/href").and_then(|h| h.as_str()).ok_or_else(|| anyhow::anyhow!("LFS object {oid} has no download"))?;
            let u = url::Url::parse(href)?;
            anyhow::ensure!(u.scheme() == "https" || (r.allow_private && u.scheme() == "http"), "LFS object {oid}: its download isn't https");
            let mut get = http.get(href);
            for (k, v) in o.pointer("/actions/download/header").and_then(|h| h.as_object()).into_iter().flatten() {
                if let Some(v) = v.as_str() {
                    get = get.header(k.as_str(), v);
                }
            }
            let mut res = get.send().await.map_err(|e| anyhow::anyhow!("LFS object {oid}: {}", e.without_url()))?;
            anyhow::ensure!(res.status().is_success(), "LFS object {oid}: {}", res.status());
            // Streamed to a file beside the pointer, checked as it comes, then put in its place.
            use sha2::Digest;
            use std::io::Write;
            let part = path.with_extension("roll-lfs");
            let mut f = std::fs::OpenOptions::new().write(true).create_new(true).open(&part)?;
            let (mut h, mut n) = (sha2::Sha256::new(), 0u64);
            while let Some(c) = res.chunk().await? {
                n += c.len() as u64;
                anyhow::ensure!(n <= *size, "LFS object {oid} is longer than its pointer says");
                h.update(&c);
                f.write_all(&c)?;
            }
            drop(f);
            if n != *size || hex::encode(h.finalize()) != *oid {
                let _ = std::fs::remove_file(&part);
                anyhow::bail!("LFS object {oid} isn't what its pointer names");
            }
            std::fs::rename(&part, path)?;
            done += 1;
        }
    }
    Ok(done)
}

/// A tree's LFS pointer files (regular files, never links): their path, sha256 and size.
/// Submodules are left out: their objects are their own repositories' LFS servers', never the
/// superproject's to answer.
fn lfs_pointers(dir: &Path) -> anyhow::Result<Vec<(std::path::PathBuf, String, u64)>> {
    let gm = dir.join(".gitmodules");
    let subs: Vec<std::path::PathBuf> = match std::fs::symlink_metadata(&gm) {
        Ok(m) if m.is_file() && m.len() <= 64 << 10 => gitmodules(&std::fs::read_to_string(&gm).unwrap_or_default()).into_iter().map(|(p, _)| dir.join(p)).collect(),
        _ => Vec::new(),
    };
    let (mut out, mut stack, mut seen) = (Vec::new(), vec![dir.to_path_buf()], 0usize);
    while let Some(d) = stack.pop() {
        if subs.contains(&d) {
            continue;
        }
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            seen += 1;
            anyhow::ensure!(seen <= MAX_FILES, "more than {MAX_FILES} files");
            let m = std::fs::symlink_metadata(e.path())?;
            if m.is_dir() {
                stack.push(e.path());
            } else if m.is_file() && m.len() <= LFS_POINTER_MAX {
                let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                if !text.starts_with(LFS_POINTER) {
                    continue;
                }
                let field = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)).map(str::trim);
                let (Some(oid), Some(size)) = (field("oid sha256:"), field("size ").and_then(|s| s.parse::<u64>().ok())) else { continue };
                if oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                    anyhow::ensure!(size <= MAX_LFS_OBJECT, "LFS object {oid} is {size} bytes, more than {MAX_LFS_OBJECT}");
                    out.push((e.path(), oid.to_ascii_lowercase(), size));
                }
            }
        }
    }
    Ok(out)
}

fn materialize_at<'a>(r: &'a Remote<'_>, sha: &'a str, dir: &'a Path, levels: Option<u8>, budget: &'a mut usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        let oid: ObjectId = sha.parse().map_err(|_| anyhow::anyhow!("{sha} isn't a commit id"))?;
        let fetched = r.fetch(&FetchArgs { wants: vec![oid], haves: Vec::new(), depth: Some(1), filter: None }).await?;
        let objects: HashMap<ObjectId, Object> = fetched.objects.into_iter().map(|o| (o.id, o)).collect();
        let d = dir.to_path_buf();
        let (mut files, links) = tokio::task::spawn_blocking(move || write_commit_links(&objects, oid, &d)).await??;
        let Some(levels) = levels.filter(|l| *l > 0 && !links.is_empty()) else { return Ok(files) };
        // `.gitmodules` as written (a regular file, never a link: the tree's own rules).
        let gm = dir.join(".gitmodules");
        let text = match std::fs::symlink_metadata(&gm) {
            Ok(m) if m.is_file() && m.len() <= 64 << 10 => std::fs::read_to_string(&gm).unwrap_or_default(),
            _ => String::new(),
        };
        let urls = gitmodules(&text);
        let base = url::Url::parse(r.url)?;
        for (path, commit) in links {
            anyhow::ensure!(*budget > 0, "more than {MAX_SUBMODULES} submodules");
            *budget -= 1;
            let Some(raw) = urls.iter().find(|(p, _)| *p == path).map(|(_, u)| u.as_str()) else { anyhow::bail!("submodule {path} isn't in .gitmodules") };
            let url = submodule_url(r.url, raw).map_err(|e| anyhow::anyhow!("submodule {path}: {e}"))?.to_string();
            check_url(&url, r.allow_private).map_err(|e| anyhow::anyhow!("submodule {path}: {}", e.trim_start_matches("source.url: ")))?;
            let same_host = url::Url::parse(&url).ok().and_then(|u| u.host_str().map(str::to_string)) == base.host_str().map(str::to_string);
            let login = r.auth.as_ref().filter(|_| same_host).map(|c| (c.username.clone(), c.password.clone()));
            let sub = Remote::new(r.allow_private, &url, login);
            let at = dir.join(&path);
            std::fs::create_dir(&at).map_err(|e| anyhow::anyhow!("submodule {path}: {e}"))?;
            files += materialize_at(&sub, &commit.to_string(), &at, Some(levels - 1), budget).await.map_err(|e| anyhow::anyhow!("submodule {path}: {e:#}"))?;
            anyhow::ensure!(files <= MAX_FILES, "more than {MAX_FILES} files");
        }
        Ok(files)
    })
}

/// `.gitmodules`' `path` and `url` of each `[submodule "…"]`.
pub(crate) fn gitmodules(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let (mut path, mut url) = (None::<String>, None::<String>);
    let mut flush = |path: &mut Option<String>, url: &mut Option<String>| {
        if let (Some(p), Some(u)) = (path.take(), url.take()) {
            out.push((p.trim_matches('/').to_string(), u));
        }
    };
    for line in text.lines().map(str::trim) {
        if line.starts_with('[') {
            flush(&mut path, &mut url);
        } else if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "path" => path = Some(v.trim().to_string()),
                "url" => url = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    flush(&mut path, &mut url);
    out
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
    write_commit_links(objects, commit, dir).map(|(n, _)| n)
}

/// A commit's tree written into `dir`, and its gitlinks (a submodule's path and pinned commit).
fn write_commit_links(objects: &HashMap<ObjectId, Object>, commit: ObjectId, dir: &Path) -> anyhow::Result<(usize, Vec<(String, ObjectId)>)> {
    let c = objects.get(&commit).filter(|o| o.kind == Kind::Commit).ok_or_else(|| anyhow::anyhow!("the fetch didn't bring commit {commit}"))?;
    let info = parse_commit(&c.data).map_err(|e| anyhow::anyhow!("{e}"))?;
    let (mut files, mut links) = (0, Vec::new());
    write_tree(objects, info.tree, dir, "", 0, &mut files, &mut links)?;
    Ok((files, links))
}

/// A tree written with the hostile-tree rules: a name is one path segment (never `.`, `..`,
/// `.git` or one with a `/`), nothing is written over (a repeated name fails), symlinks aren't
/// followed (left out: a link out of the tree would build from the host), submodules are only
/// collected (`materialize_with` fetches them when asked), and the files and depth are bounded.
fn write_tree(objects: &HashMap<ObjectId, Object>, tree: ObjectId, dir: &Path, at: &str, depth: usize, files: &mut usize, links: &mut Vec<(String, ObjectId)>) -> anyhow::Result<()> {
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
                write_tree(objects, e.oid, &path, &format!("{at}{name}/"), depth + 1, files, links)?;
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
            // A submodule: its path and pinned commit, for `materialize_with` to fetch if asked.
            mode::GITLINK => links.push((format!("{at}{name}"), e.oid)),
            // Symlinks: not followed.
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
