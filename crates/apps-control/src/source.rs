//! A deployment's source (Vercel's Source tab; docs/apps/README.md, "What's kept to roll back
//! to"): the files a folder upload (`roll up`, a dropped folder) deployed, kept with the
//! deployment until it's pruned. `GET …/deploys/{dep}/source` lists them, `?path=` reads one.
//! The bundle is unpacked by roll-fetch (its hostile-tree rules: bounded by the bytes read, no
//! entry written through a symlink) into a directory of its own for the request, and only its
//! tree is read: the environment a bundle can carry beside it never is. A repo's deployment
//! has its commit instead, an image's none.

use crate::{Apps, Error, Result};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;

/// The most files listed.
const MAX_LIST: usize = 10_000;
/// The most bytes of one file answered.
const MAX_FILE: u64 = 1 << 20;
/// Source reads at once on a node: each unpacks a bundle to disk.
static READS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);
/// What one read unpacks at most.
const LIMITS: roll_fetch::Limits = roll_fetch::Limits { max_bytes: 512 << 20, max_file_bytes: 100 << 20, max_entries: 100_000 };

/// A file named as a secret's (`.env`, a private key, a registry login): never listed or read,
/// whatever an older client put in a bundle.
fn secret_name(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    name == ".env" || name.starts_with(".env.") || name.ends_with(".pem") || name.ends_with(".key") || name.ends_with(".p12") || name.ends_with(".pfx")
        || name.starts_with("id_rsa") || name.starts_with("id_ed25519") || name.starts_with("id_ecdsa")
        || [".npmrc", ".netrc", ".pypirc", ".git-credentials", "credentials.json", "secrets.yml", "secrets.yaml", "master.key"].contains(&name.as_str())
}

impl Apps {
    /// `GET …/deploys/{dep}/source?path=`: `{files: [{path, bytes, link?}], truncated}`, or with
    /// `path` one file: `{path, bytes, text?}` (`text` when it's UTF-8 and at most 1 MiB).
    pub async fn deployment_source(self: &Arc<Self>, service: &str, env: &str, dep: &str, path: Option<&str>) -> Result<Value> {
        self.deployment(service, env, dep).await?;
        if let Some(p) = path
            && (p.is_empty() || p.len() > 1024 || p.starts_with('/') || p.split('/').any(|c| c.is_empty() || c == "." || c == ".." || c.contains(['\\', '\0'])))
        {
            return Err(Error::BadRequest("path is a relative path inside the source".into()));
        }
        if path.is_some_and(secret_name) {
            return Err(Error::NotFound(format!("{} is named as a secret's: it's never shown", path.unwrap_or_default())));
        }
        let id = dep.to_string();
        let Some(key) = self.db(move |s| s.deployment_source(&id)).await? else {
            return Err(Error::NotFound("this deployment has no kept source: a folder upload's is kept until it's pruned (a repo's is its commit, an image's has none)".into()));
        };
        let _turn = READS.acquire().await.map_err(|e| Error::Internal(e.to_string()))?;
        let bytes = match &self.deps.objects {
            Some(o) => o.get(&key).await.map_err(|e| Error::Internal(e.to_string()))?.ok_or_else(|| Error::NotFound("the source is gone".into()))?,
            None => bytes::Bytes::from(tokio::fs::read(self.cfg.home.join(&key)).await.map_err(|_| Error::NotFound("the source is gone".into()))?),
        };
        let path = path.map(str::to_string);
        tokio::task::spawn_blocking(move || -> Result<Value> {
            let dir = tempfile::Builder::new().prefix("roll-apps-source-").tempdir().map_err(|e| Error::Internal(e.to_string()))?;
            let tree = dir.path().join("tree");
            std::fs::create_dir(&tree).map_err(|e| Error::Internal(e.to_string()))?;
            roll_fetch::unpack(std::io::Cursor::new(bytes), &tree, &dir.path().join("env"), LIMITS).map_err(|e| Error::Internal(format!("the source doesn't unpack: {e}")))?;
            match path {
                Some(p) => read_one(&tree, &p),
                None => Ok(list(&tree)),
            }
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }
}

impl Apps {
    /// `GET …/deploys/{dep}/output?path=` (Vercel's Output tab): a static build's files as it
    /// serves them, read from the site's own index (no unpacking): `{files: [{path, bytes}],
    /// truncated}`, or with `path` one file `{path, bytes, text?}`. 404 for anything but a static
    /// build, or once it's pruned.
    pub async fn deployment_output(self: &Arc<Self>, service: &str, env: &str, dep: &str, path: Option<&str>) -> Result<Value> {
        let d = self.deployment(service, env, dep).await?;
        let Some(key) = d.image.as_deref().and_then(crate::sites::key) else {
            return Err(Error::NotFound("only a static build has output files (a service's are inside its image)".into()));
        };
        let site = self.site(&key).await.map_err(|_| Error::NotFound("its files are gone (pruned)".into()))?;
        let Some(p) = path else {
            let mut files: Vec<(&String, u64)> = site.files.iter().map(|(n, (_, len))| (n, *len)).collect();
            files.sort();
            let truncated = files.len() > MAX_LIST;
            let files: Vec<Value> = files.into_iter().take(MAX_LIST).map(|(n, len)| json!({ "path": n, "bytes": len })).collect();
            return Ok(json!({ "files": files, "truncated": truncated }));
        };
        let Some(&(off, len)) = site.files.get(p.trim_start_matches('/')) else {
            return Err(Error::NotFound(format!("no file {p} in the output")));
        };
        let (tar, p) = (site.tar.clone(), p.to_string());
        tokio::task::spawn_blocking(move || -> Result<Value> {
            use std::io::{Read, Seek};
            let mut out = json!({ "path": p, "bytes": len });
            if len <= MAX_FILE {
                let mut f = std::fs::File::open(&tar).map_err(|e| Error::Internal(e.to_string()))?;
                f.seek(std::io::SeekFrom::Start(off)).map_err(|e| Error::Internal(e.to_string()))?;
                let mut b = Vec::new();
                f.take(len).read_to_end(&mut b).map_err(|e| Error::Internal(e.to_string()))?;
                if let Ok(text) = String::from_utf8(b) {
                    out["text"] = json!(text);
                }
            }
            Ok(out)
        })
        .await
        .map_err(|e| Error::Internal(e.to_string()))?
    }

    /// Kept sources metered as stored data (`stored_gb_hours`), each hour once, by each service's
    /// owner, as build caches are: what object storage says each bundle holds.
    pub(crate) async fn source_pass(self: &Arc<Self>, now: i64) {
        let Some(objects) = self.deps.objects.clone() else { return };
        let Ok(kept) = self.db(|s| s.kept_sources()).await else { return };
        let kept: Vec<_> = kept.into_iter().filter(|(svc, _, _)| self.owns(svc)).collect();
        if kept.is_empty() {
            return;
        }
        let sizes: std::collections::HashMap<String, u64> = objects.list("apps/uploads/").await.unwrap_or_default().into_iter().collect();
        let mut held: std::collections::HashMap<(String, String), u64> = std::collections::HashMap::new();
        for (svc, env, key) in kept {
            *held.entry((svc, env)).or_default() += sizes.get(&key).copied().unwrap_or(0);
        }
        for ((svc, env), bytes) in held {
            let _ = self.db(move |s| s.meter_stored(&svc, &env, "source", now / 3_600_000, bytes as f64 / 1e9)).await;
        }
    }
}

/// Every file and symlink under `tree`, sorted, at most [`MAX_LIST`]; symlinks never followed.
fn list(tree: &Path) -> Value {
    let mut files = Vec::new();
    let mut stack = vec![tree.to_path_buf()];
    let (mut truncated, mut hidden) = (false, 0u64);
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let (p, Ok(m)) = (e.path(), std::fs::symlink_metadata(e.path())) else { continue };
            if m.is_dir() {
                stack.push(p);
                continue;
            }
            if files.len() >= MAX_LIST {
                truncated = true;
                break;
            }
            let rel = p.strip_prefix(tree).map(|r| r.to_string_lossy().into_owned()).unwrap_or_default();
            if secret_name(&rel) {
                hidden += 1;
                continue;
            }
            let mut f = json!({ "path": rel, "bytes": m.len() });
            if m.file_type().is_symlink() {
                f["link"] = json!(std::fs::read_link(&p).map(|t| t.to_string_lossy().into_owned()).unwrap_or_default());
            }
            files.push(f);
        }
    }
    files.sort_by(|a, b| a["path"].as_str().cmp(&b["path"].as_str()));
    json!({ "files": files, "truncated": truncated, "hidden": hidden })
}

/// One regular file's text (UTF-8, at most [`MAX_FILE`]), or only its size.
fn read_one(tree: &Path, path: &str) -> Result<Value> {
    let mut at = tree.to_path_buf();
    for c in path.split('/') {
        at.push(c);
        // Nothing reached through a symlink: each step is a directory or, last, a regular file.
        if std::fs::symlink_metadata(&at).is_ok_and(|m| m.file_type().is_symlink()) {
            return Err(Error::NotFound(format!("{path} is a link: read what it points to by its own path")));
        }
    }
    let m = std::fs::symlink_metadata(&at).map_err(|_| Error::NotFound(format!("no file {path} in the source")))?;
    if !m.is_file() {
        return Err(Error::NotFound(format!("no file {path} in the source")));
    }
    let mut out = json!({ "path": path, "bytes": m.len() });
    if m.len() <= MAX_FILE
        && let Ok(text) = std::fs::read(&at).map(String::from_utf8)
        && let Ok(text) = text
    {
        out["text"] = json!(text);
    }
    Ok(out)
}
