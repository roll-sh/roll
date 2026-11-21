//! A roll code repo's submodules (`build.submodules`; a Git URL service's are `git_source`'s):
//! each one `.gitmodules` names is written at its path, at the commit its gitlink pins. One on
//! roll code's own host (a relative URL, or roll code's Git address) comes from roll code: a public
//! repo, or a private one `build.submodule_repos` names (whoever set that could read it). One
//! anywhere else is fetched from its public URL with no login. Two levels deep at most,
//! [`MAX_SUBMODULES`] in all and [`MAX_FILES`] files between them, as a Git URL service's.

use std::path::{Component, Path};

use crate::git_source::{self, MAX_FILES, MAX_SUBMODULES, Remote};
use crate::integrations::Sources;

/// Where a submodule comes from.
#[derive(Debug, PartialEq, Eq)]
pub enum From {
    /// A roll code repo, `owner/name`.
    Repo(String),
    /// A Git URL anywhere else.
    Url(String),
}

/// A `.gitmodules` URL under `repo` (a roll code repo whose Git address is `base`/`repo`.git).
pub fn locate(base: &str, repo: &str, raw: &str) -> anyhow::Result<From> {
    let base = base.trim_end_matches('/');
    let url = git_source::submodule_url(&format!("{base}/{repo}.git"), raw)?;
    let Some(rest) = url.as_str().strip_prefix(&format!("{base}/")) else { return Ok(From::Url(url.to_string())) };
    let name = rest.trim_end_matches('/');
    let name = name.strip_suffix(".git").unwrap_or(name);
    match name.split_once('/') {
        Some((o, n)) if !o.is_empty() && !n.is_empty() && !n.contains('/') && url.query().is_none() => Ok(From::Repo(format!("{o}/{n}"))),
        _ => anyhow::bail!("{url} isn't a roll code repo"),
    }
}

/// The submodules under the tree of `repo` at `revision`, written into `dir`. How many.
pub async fn fetch(sources: &dyn Sources, repo: &str, revision: &str, dir: &Path, allowed: &[String], allow_private: bool) -> anyhow::Result<usize> {
    let (mut budget, mut files) = (MAX_SUBMODULES, 0);
    let base = sources.git_base().unwrap_or_else(|| "roll-code://code".into());
    fetch_at(sources, &base, repo, revision, dir, allowed, allow_private, 2, &mut budget, &mut files).await
}

/// The files written under `dir` (links and directories aside), counted to `limit` at most.
fn files_in(dir: &Path, limit: usize) -> anyhow::Result<usize> {
    let (mut n, mut stack) = (0, vec![dir.to_path_buf()]);
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d)? {
            let e = e?;
            let m = std::fs::symlink_metadata(e.path())?;
            if m.is_dir() {
                stack.push(e.path());
            } else {
                n += 1;
                anyhow::ensure!(n <= limit, "more than {MAX_FILES} files in its submodules");
            }
        }
    }
    Ok(n)
}

#[allow(clippy::too_many_arguments)]
fn fetch_at<'a>(sources: &'a dyn Sources, base: &'a str, repo: &'a str, revision: &'a str, dir: &'a Path, allowed: &'a [String], allow_private: bool, levels: u8, budget: &'a mut usize, files: &'a mut usize) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<usize>> + Send + 'a>> {
    Box::pin(async move {
        // `.gitmodules` as written (a regular file, never a link: the tree's own rules).
        let gm = dir.join(".gitmodules");
        let text = match std::fs::symlink_metadata(&gm) {
            Ok(m) if m.is_file() && m.len() <= 64 << 10 => std::fs::read_to_string(&gm).unwrap_or_default(),
            _ => return Ok(0),
        };
        let mut done = 0;
        for (path, raw) in git_source::gitmodules(&text) {
            // Only a path the tree has a gitlink at: one `.gitmodules` names that isn't is left out.
            let Some(commit) = sources.gitlink(repo, revision, &path).await? else { continue };
            anyhow::ensure!(*budget > 0, "more than {MAX_SUBMODULES} submodules");
            *budget -= 1;
            let at = empty_dir(dir, &path).map_err(|e| anyhow::anyhow!("submodule {path}: {e}"))?;
            match locate(base, repo, &raw).map_err(|e| anyhow::anyhow!("submodule {path}: {e}"))? {
                From::Repo(sub) => {
                    // roll code's names are case-insensitive, so is the list.
                    if !allowed.iter().any(|a| a.trim_end_matches(".git").eq_ignore_ascii_case(&sub)) && !sources.public(&sub).await? {
                        anyhow::bail!("submodule {path}: {sub} isn't public; name it in build.submodule_repos to build it with this repo");
                    }
                    sources.materialize(&sub, &commit, &at).await.map_err(|e| anyhow::anyhow!("submodule {path} ({sub}): {e:#}"))?;
                    let (a, limit) = (at.clone(), MAX_FILES - *files);
                    *files += tokio::task::spawn_blocking(move || files_in(&a, limit)).await??;
                    if levels > 1 {
                        done += fetch_at(sources, base, &sub, &commit, &at, allowed, allow_private, levels - 1, budget, files).await?;
                    }
                }
                From::Url(url) => {
                    git_source::check_url(&url, allow_private).map_err(|e| anyhow::anyhow!("submodule {path}: {}", e.trim_start_matches("source.url: ")))?;
                    let remote = Remote::new(allow_private, &url, None);
                    *files += git_source::materialize_sub(&remote, &commit, &at, levels - 1, budget).await.map_err(|e| anyhow::anyhow!("submodule {path}: {e:#}"))?;
                    anyhow::ensure!(*files <= MAX_FILES, "more than {MAX_FILES} files in its submodules");
                }
            }
            done += 1;
        }
        Ok(done)
    })
}

/// The empty directory a gitlink left at `path` (relative, plain components, no links on the way).
fn empty_dir(dir: &Path, path: &str) -> anyhow::Result<std::path::PathBuf> {
    let rel = Path::new(path);
    anyhow::ensure!(!path.is_empty() && rel.components().all(|c| matches!(c, Component::Normal(_))), "isn't a path in the tree");
    let mut at = dir.to_path_buf();
    for c in rel.components() {
        at.push(c);
        let m = std::fs::symlink_metadata(&at).map_err(|_| anyhow::anyhow!("isn't in the tree"))?;
        anyhow::ensure!(m.is_dir(), "isn't a directory in the tree");
    }
    anyhow::ensure!(std::fs::read_dir(&at)?.next().is_none(), "isn't empty");
    Ok(at)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submodule_url_is_a_roll_code_repo_or_elsewhere() {
        let b = "https://roll.sh/git";
        assert_eq!(locate(b, "acme/app", "../lib.git").unwrap(), From::Repo("acme/lib".into()));
        assert_eq!(locate(b, "acme/app", "../../oss/vendor").unwrap(), From::Repo("oss/vendor".into()));
        assert_eq!(locate(b, "acme/app", "https://roll.sh/git/oss/x.git").unwrap(), From::Repo("oss/x".into()));
        assert_eq!(locate(b, "acme/app", "https://github.com/oss/x.git").unwrap(), From::Url("https://github.com/oss/x.git".into()));
        // Above roll code's Git address isn't one of its repos, and neither is a path inside one.
        assert_eq!(locate(b, "acme/app", "../../../x.git").unwrap(), From::Url("https://roll.sh/x.git".into()));
        assert!(locate(b, "acme/app", "./sub.git").is_err());
    }
}
