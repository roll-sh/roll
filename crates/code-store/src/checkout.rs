//! Moving a materialized tree from one changeset to another by applying the diff between them,
//! instead of writing the whole tree again (the merge queue tests a new tip every batch).
//!
//! The same rules as [`Store::materialize`] hold for hostile trees: nothing is ever written
//! through a symlink, and anything on disk that doesn't look like what the old tree says is an
//! error, so the caller falls back to a fresh materialize.

use crate::ids::ChangesetId;
use crate::model::Mode;
use crate::read::FileDiff;
use crate::Store;
use std::path::{Path, PathBuf};

impl Store {
    /// Bring `dir`, which holds exactly the tree of `from` (as [`Store::materialize`] wrote it),
    /// to the tree of `to`. Returns how many files were written or removed. On error `dir` is in
    /// an unknown state: materialize it again from scratch.
    pub async fn checkout_delta(&self, repo: &str, from: &ChangesetId, to: &ChangesetId, dir: &Path) -> anyhow::Result<u64> {
        if from == to {
            return Ok(0);
        }
        let diffs = self.compare(repo, from, to)?;
        let mut touched = 0;
        // Removals first: a file can become a directory and the other way round.
        for d in diffs.iter().filter(|d| d.old_mode.is_some() && kind_changed(d)) {
            let p = parent_dirs_ok(dir, d.path.as_str()).await?;
            let meta = tokio::fs::symlink_metadata(&p).await.map_err(|e| anyhow::anyhow!("{}: {e}", d.path))?;
            match d.old_mode {
                Some(Mode::Gitlink) if meta.is_dir() => tokio::fs::remove_dir(&p).await?,
                Some(Mode::Gitlink) => anyhow::bail!("{} should be a submodule directory", d.path),
                _ if meta.is_dir() => anyhow::bail!("{} is a directory on disk", d.path),
                _ => tokio::fs::remove_file(&p).await?,
            }
            touched += 1;
            prune_empty(dir, &p).await;
        }
        for d in diffs.iter().filter(|d| d.new_mode.is_some()) {
            let (Some(mode), Some(oid)) = (d.new_mode, d.new_oid) else { continue };
            let p = make_parents(dir, d.path.as_str()).await?;
            match tokio::fs::symlink_metadata(&p).await {
                Ok(m) if m.is_dir() && mode == Mode::Gitlink => continue,
                Ok(m) if m.is_dir() => anyhow::bail!("{} is a directory on disk", d.path),
                // Never write through what's there (it may be a symlink): replace it.
                Ok(_) => tokio::fs::remove_file(&p).await?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e.into()),
            }
            touched += 1;
            if mode == Mode::Gitlink {
                tokio::fs::create_dir(&p).await?;
                continue;
            }
            let data = self.get_blob(repo, &oid).await?.ok_or_else(|| anyhow::anyhow!("blob {oid} missing"))?;
            write_file(&p, mode, &data).await?;
        }
        Ok(touched)
    }
}

/// Whether the old entry has to go before the new one is written: deleted, or a different kind
/// (file, symlink, submodule). Plain content and exec-bit changes are rewritten in place.
fn kind_changed(d: &FileDiff) -> bool {
    let kind = |m: Mode| match m {
        Mode::Regular | Mode::Executable => 0,
        Mode::Symlink => 1,
        Mode::Gitlink => 2,
    };
    match (d.old_mode, d.new_mode) {
        (Some(_), None) => true,
        (Some(a), Some(b)) => kind(a) != kind(b),
        _ => false,
    }
}

/// `dir/path`, after checking every directory on the way is a real directory (not a symlink).
async fn parent_dirs_ok(dir: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let mut at = dir.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    for c in &parts[..parts.len() - 1] {
        at.push(c);
        let m = tokio::fs::symlink_metadata(&at).await.map_err(|e| anyhow::anyhow!("{}: {e}", at.display()))?;
        if !m.is_dir() {
            anyhow::bail!("{} isn't a directory on disk", at.display());
        }
    }
    at.push(parts[parts.len() - 1]);
    Ok(at)
}

/// Like [`parent_dirs_ok`], creating missing directories.
async fn make_parents(dir: &Path, path: &str) -> anyhow::Result<PathBuf> {
    let mut at = dir.to_path_buf();
    let parts: Vec<&str> = path.split('/').collect();
    for c in &parts[..parts.len() - 1] {
        at.push(c);
        match tokio::fs::symlink_metadata(&at).await {
            Ok(m) if m.is_dir() => {}
            Ok(_) => anyhow::bail!("{} isn't a directory on disk", at.display()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => tokio::fs::create_dir(&at).await?,
            Err(e) => return Err(e.into()),
        }
    }
    at.push(parts[parts.len() - 1]);
    Ok(at)
}

/// Remove directories left empty above `removed`, up to (not including) `dir`.
async fn prune_empty(dir: &Path, removed: &Path) {
    let mut at = removed.parent();
    while let Some(p) = at {
        if p == dir || !p.starts_with(dir) || tokio::fs::remove_dir(p).await.is_err() {
            return;
        }
        at = p.parent();
    }
}

pub(crate) async fn write_file(p: &Path, mode: Mode, data: &[u8]) -> anyhow::Result<()> {
    if mode == Mode::Symlink {
        #[cfg(unix)]
        {
            let target = String::from_utf8_lossy(data).into_owned();
            tokio::fs::symlink(target, p).await?;
            return Ok(());
        }
    }
    tokio::fs::write(p, data).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perm = if mode == Mode::Executable { 0o755 } else { 0o644 };
        tokio::fs::set_permissions(p, std::fs::Permissions::from_mode(perm)).await?;
    }
    Ok(())
}
