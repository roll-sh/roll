//! `roll secrets import --from doppler|infisical` with the source's structure (docs/secrets.md,
//! "Moving from another manager"): environments (Doppler configs), Infisical's folder tree and
//! secret imports, comments, and references kept as roll references rather than the values they
//! compute to today.
//!
//! Read here, with the user's own token, never through roll's server. Values never go in an
//! error, a report or a dry run: those name environments, folders and keys only. Answers are read
//! up to 16 MiB; no redirects are followed, so a token never goes to another host.

use anyhow::{Context, bail, ensure};
use serde_json::Value;

/// The most an answer may hold.
const ANSWER_MAX: usize = 16 << 20;
const ENVIRONMENTS_MAX: usize = 50;
const FOLDERS_MAX: usize = 2_000;
const KEYS_MAX: usize = 10_000;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Vendor {
    Doppler,
    Infisical,
}

impl Vendor {
    pub fn parse(from: &str) -> Option<Vendor> {
        match from {
            "doppler" => Some(Vendor::Doppler),
            "infisical" => Some(Vendor::Infisical),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Vendor::Doppler => "doppler",
            Vendor::Infisical => "infisical",
        }
    }
}

/// Which of the source's environments go where.
pub enum Which {
    /// `--source` names one (Doppler `project/config`, Infisical `workspace/env[/path]`), into
    /// the command's environment.
    One { into: String },
    /// `--map source=target,…`: those, `--source` naming the project.
    Map(Vec<(String, String)>),
    /// `--all-environments`: every one the project lists, each into the roll environment `--map`
    /// names, else its own name (`dev_api` → `dev-api`).
    All(Vec<(String, String)>),
}

pub struct Request {
    pub vendor: Vendor,
    pub source: String,
    pub which: Which,
    /// Infisical: every folder under the source's, each into its own folder.
    pub recursive: bool,
    /// The folder it goes into (`--path`).
    pub into_path: String,
}

/// A key to write. Its value is never printed, `Debug`ged or put in an error.
pub struct Key {
    pub key: String,
    pub value: String,
    pub comment: String,
    /// The roll references its value now holds (`development.HOST`).
    pub refs: Vec<String>,
    /// Why it comes in as its computed value instead (a reference roll can't hold): names only.
    pub flattened: Vec<String>,
}

/// A roll folder import (`{environment, path}`), in roll's order: the first one listed wins.
#[derive(Clone, PartialEq, Eq)]
pub struct Import {
    pub environment: String,
    pub path: String,
}

pub struct Folder {
    /// Under the import's `--path` (`""`: the path itself; `db/replica`).
    pub folder: String,
    pub keys: Vec<Key>,
    pub imports: Vec<Import>,
    /// What isn't brought, and why: names only.
    pub left_out: Vec<String>,
}

pub struct Environment {
    /// The source's name (a Doppler config, an Infisical environment's slug).
    pub source: String,
    /// The roll environment's slug.
    pub target: String,
    pub folders: Vec<Folder>,
}

pub struct Plan {
    pub environments: Vec<Environment>,
}

impl Plan {
    pub fn keys(&self) -> usize {
        self.environments.iter().flat_map(|e| &e.folders).map(|f| f.keys.len()).sum()
    }
}

/// A key as the source stores it: its raw value, which may hold the source's references.
struct Raw {
    key: String,
    raw: String,
    comment: String,
}

/// What the source keeps in one folder.
struct SourceFolder {
    /// The source's own path (`/`, `/api/db`).
    path: String,
    keys: Vec<Raw>,
    /// Infisical's secret imports (environment slug, path, replication), in Infisical's order:
    /// the bottom-most wins there.
    imports: Vec<(String, String, bool)>,
    left_out: Vec<String>,
}

pub async fn read(req: &Request) -> anyhow::Result<Plan> {
    let http = reqwest::Client::builder().timeout(std::time::Duration::from_secs(30)).redirect(reqwest::redirect::Policy::none()).build()?;
    let src = match req.vendor {
        Vendor::Doppler => Source::doppler(http)?,
        Vendor::Infisical => Source::infisical(http)?,
    };
    let vendor = req.vendor.name();
    // The project, the source's folder, and which environments go where.
    let (project, root, map) = match &req.which {
        Which::One { into } => {
            let (project, rest) = req.source.split_once('/').with_context(|| match req.vendor {
                Vendor::Doppler => "--source is project/config",
                Vendor::Infisical => "--source is workspace-id/environment[/path]",
            })?;
            let (name, root) = match req.vendor {
                Vendor::Doppler => (rest.to_string(), "/".to_string()),
                Vendor::Infisical => match rest.split_once('/') {
                    Some((e, p)) => (e.to_string(), folder_path(p)),
                    None => (rest.to_string(), "/".into()),
                },
            };
            ensure!(!project.is_empty() && !name.is_empty(), "--source is {}", if req.vendor == Vendor::Doppler { "project/config" } else { "workspace-id/environment[/path]" });
            (project.to_string(), root, vec![(name, into.clone())])
        }
        Which::Map(pairs) => (project_only(&req.source)?, "/".into(), pairs.clone()),
        Which::All(overrides) => {
            let project = project_only(&req.source)?;
            let names = src.environments(&project).await?;
            ensure!(!names.is_empty(), "{vendor} {project} lists no environments");
            ensure!(names.len() <= ENVIRONMENTS_MAX, "{vendor} {project} has {} environments: {ENVIRONMENTS_MAX} at most at once; --map the ones to import", names.len());
            for (from, _) in overrides {
                ensure!(names.contains(from), "--map {from}: {vendor} {project} has no {from} (it has {})", names.join(", "));
            }
            let mut map = vec![];
            for n in names {
                let target = match overrides.iter().find(|(f, _)| *f == n) {
                    Some((_, t)) => t.clone(),
                    None => {
                        let t = n.to_ascii_lowercase().replace('_', "-");
                        ensure!(roll_secrets_ref::is_slug(&t), "{n} can't be a roll environment's name: --map {n}=<environment>");
                        t
                    }
                };
                map.push((n, target));
            }
            (project, "/".into(), map)
        }
    };
    let mut targets: Vec<&str> = map.iter().map(|(_, t)| t.as_str()).collect();
    targets.sort_unstable();
    if let Some(w) = targets.windows(2).find(|w| w[0] == w[1]) {
        bail!("two {vendor} environments go into {}: --map one elsewhere", w[0]);
    }
    let refs = Refs { vendor: req.vendor, project: &project, map: &map, root: &root, into: &req.into_path };
    let mut plan = Plan { environments: vec![] };
    let mut total = 0usize;
    for (name, target) in &map {
        let folders = src.folders(&project, name, &root, req.recursive).await.with_context(|| format!("reading {vendor} {project} {name}"))?;
        let mut env = Environment { source: name.clone(), target: target.clone(), folders: vec![] };
        for f in folders {
            let rel = relative(&root, &f.path).with_context(|| format!("{}: outside {root}", f.path))?;
            let mut keys = vec![];
            let mut flatten = vec![];
            for r in f.keys {
                total += 1;
                ensure!(total <= KEYS_MAX, "more than {KEYS_MAX} keys: import in parts (a narrower --source or --map)");
                let (value, to, problems) = refs.rewrite(&r.raw);
                if !problems.is_empty() {
                    flatten.push(r.key.clone());
                }
                keys.push(Key { key: r.key, value, comment: r.comment, refs: to, flattened: problems });
            }
            if !flatten.is_empty() {
                // What it computes to today, read only for the keys roll can't reference.
                let computed = src.computed(&project, name, &f.path, &flatten).await?;
                for k in keys.iter_mut().filter(|k| !k.flattened.is_empty()) {
                    k.value = computed.iter().find(|(n, _)| *n == k.key).map(|(_, v)| v.clone()).with_context(|| format!("{vendor} {name}: {} has no computed value the token can read", k.key))?;
                    k.refs.clear();
                }
            }
            let mut imports = vec![];
            let mut left_out = f.left_out;
            // Roll's first import wins, Infisical's last: the same order, reversed.
            for (env_slug, path, replication) in f.imports.into_iter().rev() {
                let what = format!("the import of {env_slug}:{path}");
                if replication {
                    left_out.push(format!("{what}: a replication, which copies values into the folder; roll imports read through instead"));
                    continue;
                }
                let Some((_, to)) = map.iter().find(|(s, _)| *s == env_slug) else {
                    left_out.push(format!("{what}: {env_slug} isn't imported here (--map it, or --all-environments)"));
                    continue;
                };
                let Some(inner) = relative(&root, &folder_path(&path)) else {
                    left_out.push(format!("{what}: outside {root}"));
                    continue;
                };
                imports.push(Import { environment: to.clone(), path: under(&req.into_path, &inner) });
            }
            env.folders.push(Folder { folder: rel.trim_start_matches('/').to_string(), keys, imports, left_out });
        }
        plan.environments.push(env);
    }
    Ok(plan)
}

/// `--source` as a project alone (`--map`, `--all-environments`).
fn project_only(source: &str) -> anyhow::Result<String> {
    let p = source.trim_end_matches('/');
    ensure!(!p.is_empty() && !p.contains('/'), "with --map or --all-environments, --source is the project alone (Doppler's project, Infisical's workspace id)");
    Ok(p.to_string())
}

/// `p` as an absolute folder path (`/`, `/api/db`).
fn folder_path(p: &str) -> String {
    let p = p.trim_matches('/');
    if p.is_empty() { "/".into() } else { format!("/{p}") }
}

/// `path` relative to `root` (`/` for `root` itself); `None` outside it.
fn relative(root: &str, path: &str) -> Option<String> {
    if root == "/" {
        return Some(path.to_string());
    }
    match path.strip_prefix(root) {
        Some("") => Some("/".into()),
        Some(rest) if rest.starts_with('/') => Some(rest.to_string()),
        _ => None,
    }
}

/// The roll folder `rel` (relative to the source's folder) goes into under `--path`.
fn under(into: &str, rel: &str) -> String {
    folder_path(&format!("{}/{}", into.trim_end_matches('/'), rel.trim_start_matches('/')))
}

/// The source's references, rewritten into roll's.
struct Refs<'a> {
    vendor: Vendor,
    project: &'a str,
    map: &'a [(String, String)],
    /// The source's folder that goes into `into`.
    root: &'a str,
    into: &'a str,
}

/// Where a source reference points in roll.
enum Target {
    /// `${KEY}`: the same folder.
    Same(String),
    /// `${env.folder.KEY}`.
    At(String),
}

impl Refs<'_> {
    /// `inner` (between `${` and `}`) as the source reads it: `None` when it isn't a reference
    /// there; `Err` with why roll can't hold it, naming names only.
    fn target(&self, inner: &str) -> Option<Result<Target, String>> {
        let name = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
        let (cross, body) = match inner.strip_prefix('@') {
            Some(b) if self.vendor == Vendor::Infisical => (true, b),
            _ => (false, inner),
        };
        let segs: Vec<&str> = body.split('.').collect();
        if !segs.iter().all(|s| name(s)) {
            return None;
        }
        if cross {
            return Some(Err(format!("{inner}: another Infisical project; a roll reference stays in its project")));
        }
        let (env, folders, key): (&str, Vec<&str>, &str) = match (self.vendor, segs.as_slice()) {
            (_, [key]) => {
                return Some(if roll_secrets_ref::is_key(key) { Ok(Target::Same(key.to_string())) } else { Err(format!("{inner}: not a roll key")) });
            }
            (Vendor::Doppler, [config, key]) => (*config, vec![], *key),
            (Vendor::Doppler, [project, config, key]) if *project == self.project => (*config, vec![], *key),
            (Vendor::Doppler, [_, _, _]) => return Some(Err(format!("{inner}: another Doppler project; a roll reference stays in its project"))),
            (Vendor::Doppler, _) => return None,
            (Vendor::Infisical, [env, rest @ .., key]) => (*env, rest.to_vec(), *key),
            (Vendor::Infisical, []) => return None,
        };
        let Some((_, to)) = self.map.iter().find(|(s, _)| s.as_str() == env) else {
            return Some(Err(format!("{inner}: {env} isn't imported here (--map it, or --all-environments)")));
        };
        let Some(rel) = relative(self.root, &folder_path(&folders.join("/"))) else {
            return Some(Err(format!("{inner}: outside {}", self.root)));
        };
        let path = under(self.into, &rel);
        let segments: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        if !roll_secrets_ref::is_key(key) || segments.len() > roll_secrets_ref::DEPTH_MAX || !segments.iter().all(|s| roll_secrets_ref::is_segment(s)) {
            return Some(Err(format!("{inner}: not a roll key or folder")));
        }
        let mut at = vec![to.as_str()];
        at.extend(segments);
        at.push(key);
        Some(Ok(Target::At(at.join("."))))
    }

    /// `raw` (in environment `env`) with its references in roll's syntax, the roll references it
    /// holds, and why it can't keep the ones it can't (then it comes in as its computed value).
    /// Text roll would read as a reference is escaped (`$${`), so it stays text.
    fn rewrite(&self, raw: &str) -> (String, Vec<String>, Vec<String>) {
        let mut out = String::with_capacity(raw.len());
        let (mut refs, mut problems) = (vec![], vec![]);
        let text = |out: &mut String, t: &str| out.push_str(&t.replace("${", "$${"));
        let mut rest = raw;
        while let Some(i) = rest.find("${") {
            let Some(end) = rest[i + 2..].find('}') else { break };
            let inner = &rest[i + 2..i + 2 + end];
            match self.target(inner) {
                None => text(&mut out, &rest[..i + 3 + end]),
                Some(t) => {
                    text(&mut out, &rest[..i]);
                    if out.ends_with('$') {
                        // `$${` is roll's literal `${`: no way to write a `$` right before one.
                        problems.push(format!("{inner}: a `$` right before it"));
                    }
                    match t {
                        Ok(Target::Same(k)) => {
                            out.push_str(&format!("${{{k}}}"));
                            refs.push(k);
                        }
                        Ok(Target::At(r)) => {
                            out.push_str(&format!("${{{r}}}"));
                            refs.push(r);
                        }
                        Err(why) => problems.push(why),
                    }
                }
            }
            rest = &rest[i + 3 + end..];
        }
        text(&mut out, rest);
        (out, refs, problems)
    }
}

/// A source's API, signed in with the user's token.
struct Source {
    vendor: Vendor,
    http: reqwest::Client,
    host: String,
    token: String,
}

fn var(k: &str) -> Option<String> {
    std::env::var(k).ok().filter(|v| !v.trim().is_empty())
}

fn enc(s: &str) -> String {
    urlencoding::encode(s).into_owned()
}

impl Source {
    fn doppler(http: reqwest::Client) -> anyhow::Result<Source> {
        let host = var("DOPPLER_API_HOST").unwrap_or_else(|| "https://api.doppler.com".into());
        Ok(Source { vendor: Vendor::Doppler, http, host: host.trim_end_matches('/').into(), token: var("DOPPLER_TOKEN").context("set DOPPLER_TOKEN")? })
    }

    fn infisical(http: reqwest::Client) -> anyhow::Result<Source> {
        let host = var("INFISICAL_API_URL").unwrap_or_else(|| "https://app.infisical.com".into());
        Ok(Source { vendor: Vendor::Infisical, http, host: host.trim_end_matches('/').into(), token: var("INFISICAL_TOKEN").context("set INFISICAL_TOKEN")? })
    }

    /// `path`'s answer, read up to 16 MiB; `None` for a 404. Never the body in an error: it may
    /// hold values.
    async fn get(&self, path: &str, what: &str) -> anyhow::Result<Option<Value>> {
        let what = format!("{} {what}", self.vendor.name());
        let mut res = self.http.get(format!("{}{path}", self.host)).bearer_auth(&self.token).send().await.with_context(|| what.clone())?;
        if res.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        ensure!(res.status().is_success(), "{what}: {}", res.status());
        let mut body = Vec::new();
        while let Some(chunk) = res.chunk().await.with_context(|| what.clone())? {
            ensure!(body.len() + chunk.len() <= ANSWER_MAX, "{what}: the answer is over 16 MiB");
            body.extend_from_slice(&chunk);
        }
        Ok(Some(serde_json::from_slice(&body).with_context(|| format!("{what}: not JSON"))?))
    }

    async fn need(&self, path: &str, what: &str) -> anyhow::Result<Value> {
        self.get(path, what).await?.with_context(|| format!("{} {what}: not found", self.vendor.name()))
    }

    /// The project's environments: Doppler's configs, Infisical's environments' slugs.
    async fn environments(&self, project: &str) -> anyhow::Result<Vec<String>> {
        let mut out = vec![];
        match self.vendor {
            Vendor::Doppler => {
                for page in 1..=20 {
                    let v = self.need(&format!("/v3/configs?project={}&page={page}&per_page=100", enc(project)), &format!("{project}'s configs")).await?;
                    let configs = v["configs"].as_array().cloned().unwrap_or_default();
                    out.extend(configs.iter().filter_map(|c| c["name"].as_str().map(String::from)));
                    if configs.len() < 100 {
                        break;
                    }
                }
            }
            Vendor::Infisical => {
                let what = format!("{project}'s environments");
                // The current route, then the one older servers have.
                let v = match self.get(&format!("/api/v1/projects/{}", enc(project)), &what).await? {
                    Some(v) => v["project"]["environments"].clone(),
                    None => self.need(&format!("/api/v1/workspace/{}", enc(project)), &what).await?["workspace"]["environments"].clone(),
                };
                out.extend(v.as_array().into_iter().flatten().filter_map(|e| e["slug"].as_str().map(String::from)));
            }
        }
        Ok(out)
    }

    /// The folders of environment `env` from `root`: only it, or with `recursive` (Infisical)
    /// every folder under it, at most 8 deep and 2,000 in all.
    async fn folders(&self, project: &str, env: &str, root: &str, recursive: bool) -> anyhow::Result<Vec<SourceFolder>> {
        if self.vendor == Vendor::Doppler {
            return Ok(vec![SourceFolder { path: "/".into(), keys: self.doppler_secrets(project, env).await?, imports: vec![], left_out: vec![] }]);
        }
        let mut out = vec![];
        let mut queue = vec![(root.to_string(), 0usize)];
        while let Some((path, depth)) = queue.pop() {
            ensure!(out.len() < FOLDERS_MAX, "infisical {env}: more than {FOLDERS_MAX} folders; import in parts (a narrower --source)");
            let q = format!("projectId={}&environment={}&path={}", enc(project), enc(env), enc(&path));
            let v = self.need(&format!("/api/v2/folders?{q}"), &format!("{env}:{path}'s folders")).await?;
            let mut subs: Vec<String> = v["folders"].as_array().into_iter().flatten().filter_map(|f| f["name"].as_str().map(String::from)).collect();
            subs.sort();
            let mut left_out = vec![];
            if recursive {
                for s in &subs {
                    ensure!(roll_secrets_ref::is_segment(s), "infisical {env}:{path}: the folder {s:?} can't be a roll folder (letters, digits, _ and -)");
                    ensure!(depth < roll_secrets_ref::DEPTH_MAX, "infisical {env}:{path}: more than {} levels of folders", roll_secrets_ref::DEPTH_MAX);
                    queue.push((format!("{}/{s}", path.trim_end_matches('/')), depth + 1));
                }
            } else if !subs.is_empty() {
                left_out.push(format!("{} folder{} under it ({}): --recursive brings them", subs.len(), if subs.len() == 1 { "" } else { "s" }, subs.join(", ")));
            }
            let keys = self.infisical_secrets(project, env, &path, false).await?.into_iter().map(|(key, raw, comment)| Raw { key, raw, comment }).collect();
            let v = self.need(&format!("/api/v2/secret-imports?{q}"), &format!("{env}:{path}'s imports")).await?;
            let mut list: Vec<(i64, String, String, bool)> = v["secretImports"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| Some((i["position"].as_i64().unwrap_or(0), i["importEnv"]["slug"].as_str()?.to_string(), i["importPath"].as_str()?.to_string(), i["isReplication"] == true)))
                .collect();
            list.sort_by_key(|i| i.0);
            out.push(SourceFolder { path, keys, imports: list.into_iter().map(|(_, e, p, r)| (e, p, r)).collect(), left_out });
        }
        out.sort_by(|a, b| a.path.cmp(&b.path));
        Ok(out)
    }

    /// A Doppler config's raw values, with their notes. Doppler's own `DOPPLER_*` keys are left
    /// out; a value the token can't read stops it, naming the key.
    async fn doppler_secrets(&self, project: &str, config: &str) -> anyhow::Result<Vec<Raw>> {
        let v = self.secrets_doppler(project, config).await?;
        let mut out = vec![];
        let mut hidden = vec![];
        for (k, x) in v["secrets"].as_object().into_iter().flatten() {
            if k.starts_with("DOPPLER_") {
                continue;
            }
            match &x["raw"] {
                Value::String(raw) => out.push(Raw { key: k.clone(), raw: raw.clone(), comment: x["note"].as_str().unwrap_or_default().to_string() }),
                _ => hidden.push(k.clone()),
            }
        }
        ensure!(hidden.is_empty(), "doppler {project}/{config}: the token can't read {} (restricted)", hidden.join(", "));
        Ok(out)
    }

    async fn secrets_doppler(&self, project: &str, config: &str) -> anyhow::Result<Value> {
        let path = format!("/v3/configs/config/secrets?project={}&config={}&include_dynamic_secrets=false&include_managed_secrets=false", enc(project), enc(config));
        self.need(&path, &format!("{project}/{config}")).await
    }

    /// An Infisical folder's secrets: (key, value, comment), raw or with its references expanded.
    /// Personal overrides are left out; a value the token can't view stops it, naming the key.
    async fn infisical_secrets(&self, project: &str, env: &str, path: &str, expand: bool) -> anyhow::Result<Vec<(String, String, String)>> {
        let url = format!("/api/v3/secrets/raw?workspaceId={}&environment={}&secretPath={}&expandSecretReferences={expand}&include_imports=false&recursive=false", enc(project), enc(env), enc(path));
        let v = self.need(&url, &format!("{env}:{path}")).await?;
        let mut out = vec![];
        let mut hidden = vec![];
        for x in v["secrets"].as_array().into_iter().flatten() {
            let Some(k) = x["secretKey"].as_str() else { continue };
            if x["type"] == "personal" {
                continue;
            }
            match &x["secretValue"] {
                Value::String(s) if x["secretValueHidden"] != true => out.push((k.to_string(), s.clone(), x["secretComment"].as_str().unwrap_or_default().to_string())),
                _ => hidden.push(k.to_string()),
            }
        }
        ensure!(hidden.is_empty(), "infisical {env}:{path}: the token can't view {}", hidden.join(", "));
        Ok(out)
    }

    /// What `keys` compute to today in a folder: for the references roll can't hold.
    async fn computed(&self, project: &str, env: &str, path: &str, keys: &[String]) -> anyhow::Result<Vec<(String, String)>> {
        let all: Vec<(String, String)> = match self.vendor {
            Vendor::Doppler => self.secrets_doppler(project, env).await?["secrets"].as_object().into_iter().flatten().filter_map(|(k, x)| Some((k.clone(), x["computed"].as_str()?.to_string()))).collect(),
            Vendor::Infisical => self.infisical_secrets(project, env, path, true).await?.into_iter().map(|(k, v, _)| (k, v)).collect(),
        };
        Ok(all.into_iter().filter(|(k, _)| keys.contains(k)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refs<'a>(vendor: Vendor, map: &'a [(String, String)], root: &'a str, into: &'a str) -> Refs<'a> {
        Refs { vendor, project: "shop", map, root, into }
    }

    #[test]
    fn source_references_become_roll_references_or_say_why_not() {
        let map = vec![("dev".to_string(), "development".to_string()), ("prd".to_string(), "production".to_string())];
        let d = refs(Vendor::Doppler, &map, "/", "/");
        let (v, to, bad) = d.rewrite("postgres://${USER}:${dev.PASS}@${shop.dev.HOST}/x");
        assert_eq!((v.as_str(), to, bad.len()), ("postgres://${USER}:${development.PASS}@${development.HOST}/x", vec!["USER".to_string(), "development.PASS".into(), "development.HOST".into()], 0));
        let (_, _, bad) = d.rewrite("${stg.KEY} ${other.dev.KEY}");
        assert!(bad[0].contains("stg isn't imported") && bad[1].contains("another Doppler project"), "{bad:?}");
        // Text roll would read as a reference stays text; `$` before one can't be written.
        assert_eq!(d.rewrite("a ${x y} b").0, "a $${x y} b");
        assert!(!d.rewrite("$${KEY}").2.is_empty());
        let i = refs(Vendor::Infisical, &map, "/api", "/app");
        assert_eq!(i.rewrite("${prd.api.db.HOST}").0, "${production.app.db.HOST}");
        assert!(i.rewrite("${prd.web.HOST}").2[0].contains("outside /api"));
        assert!(i.rewrite("${@billing.prd.KEY}").2[0].contains("another Infisical project"));
    }
}
