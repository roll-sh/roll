//! Sandboxes (Fly Machines', Vercel Sandbox's): short-lived containers a person, an agent or the
//! environment's own code starts by API to run something in: the live image (or a public one)
//! kept up for its TTL, commands run in it for their exit code and output, files written and read.
//!
//! - By default a sandbox gets none of the environment's variables or files, and a network of its
//!   own: code nobody vetted (an agent's, a user's) reaches the internet and nothing of the
//!   project. `variables: true` gives it the live deployment's (as a console has them: its links'
//!   databases are reached through them), `network: "environment"` the environment's network
//!   (its other services by their private names). A closed environment's sandboxes are always
//!   on its network: they reach nothing but roll, as its code does.
//! - At most [`MAX_PER_ENV`] at once, each for 60 s to a day ([`MAX_TTL_S`]); the owner's minute
//!   sweep removes one past its end, and a takeover keeps it until then. Metered as a console is,
//!   for as long as it ran; refused while the service is capped or the environment paused.
//! - Its container carries the service's and the environment's labels: deleting either takes it.

use crate::{Apps, Error, Result};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// Sandboxes an environment holds at once.
pub const MAX_PER_ENV: usize = 10;
pub const MIN_TTL_S: u32 = 60;
pub const MAX_TTL_S: u32 = 86_400;
const DEFAULT_TTL_S: u32 = 600;
/// What a command's output keeps of each stream, and a file read or written at most.
pub const MAX_OUTPUT: usize = 1 << 20;
pub const MAX_FILE: usize = 1 << 20;

#[derive(Clone, Debug, Serialize)]
pub struct Sandbox {
    pub id: String,
    pub service: String,
    pub environment: String,
    pub image: String,
    pub cpu: f64,
    pub memory_mb: u32,
    /// It has the live deployment's variables and files.
    pub variables: bool,
    /// `none` (the runtime's own network: the internet, nothing of the project) or `environment`.
    pub network: String,
    pub created_at: i64,
    pub expires_at: i64,
}

impl Apps {
    /// `POST …/environments/{env}/sandboxes`: one made and started.
    pub async fn create_sandbox(self: &Arc<Self>, service: &str, env: &str, body: Value) -> Result<Sandbox> {
        let o = body.as_object().cloned().unwrap_or_default();
        if let Some(k) = o.keys().find(|k| !["image", "ttl_s", "cpu", "memory_mb", "variables", "network"].contains(&k.as_str())) {
            return Err(Error::BadRequest(format!("unknown field {k}")));
        }
        self.refuse_capped(service)?;
        self.refuse_paused(service, env).await?;
        let ttl = match o.get("ttl_s") {
            None => DEFAULT_TTL_S,
            Some(v) => v.as_u64().filter(|t| (MIN_TTL_S as u64..=MAX_TTL_S as u64).contains(t)).ok_or_else(|| Error::BadRequest(format!("ttl_s is {MIN_TTL_S}–{MAX_TTL_S} seconds")))? as u32,
        };
        let cpu = match o.get("cpu") {
            None => 0.5,
            Some(v) => v.as_f64().filter(|c| (0.25..=4.0).contains(c)).ok_or_else(|| Error::BadRequest("cpu is 0.25–4".into()))?,
        };
        let memory_mb = match o.get("memory_mb") {
            None => 512,
            Some(v) => v.as_u64().filter(|m| (256..=8192).contains(m)).ok_or_else(|| Error::BadRequest("memory_mb is 256–8192".into()))? as u32,
        };
        let variables = match o.get("variables") {
            None => false,
            Some(Value::Bool(b)) => *b,
            Some(_) => return Err(Error::BadRequest("variables is a boolean".into())),
        };
        let network = match o.get("network").map(|n| n.as_str()) {
            None => "none",
            Some(Some("none")) => "none",
            Some(Some("environment")) => "environment",
            Some(_) => return Err(Error::BadRequest("network is \"none\" (the runtime's own: the internet, nothing of the project) or \"environment\"".into())),
        };
        let image = match o.get("image") {
            None => None,
            Some(Value::String(i)) if !i.is_empty() && i.len() <= 255 && !i.contains(char::is_whitespace) => Some(i.clone()),
            Some(_) => return Err(Error::BadRequest("image is an image reference (nginx:1.27, ghcr.io/acme/tool:2)".into())),
        };
        // The live deployment's instance, as a console is made: its image unless another is named.
        let live = self.load_live(service, env).await?;
        let tpl = live.template().ok_or_else(|| Error::Conflict(format!("{env} has nothing deployed to make a sandbox of")))?;
        // A closed environment's code reaches nothing but roll: nor does a sandbox of it (its
        // image is the environment's).
        if tpl.spec.closed && network == "none" && o.contains_key("network") {
            return Err(Error::BadRequest(format!("{env} is closed (egress roll): its sandboxes are on its network")));
        }
        let network = if tpl.spec.closed { "environment" } else { network };
        let mut tpl = (*tpl).clone();
        let id = crate::model::new_id("sbx");
        let name = sandbox_name(service, env, &id);
        let spec = &mut tpl.spec;
        if let Some(i) = &image {
            spec.image = i.clone();
            // A public image: the deployment's registry login is the deployment's.
            spec.registry = None;
        }
        // Up for its TTL even if nothing removes it (the runtime's init reaps what commands leave).
        spec.command = Some(format!("exec sleep {ttl}"));
        (spec.port, spec.health, spec.volume, spec.idle_stop_s) = (None, None, None, None);
        spec.ports.clear();
        spec.aliases.clear();
        spec.sidecars.clear();
        (spec.cpu, spec.memory_mb, spec.udp) = (cpu, memory_mb, false);
        if !variables {
            spec.vars.clear();
            spec.files.clear();
        }
        if network == "none" {
            // A network of its own with a route out: the internet, none of the project's.
            let net = network_name(&name);
            self.deps.runtime.ensure_network(&net, service, false).await.map_err(|e| Error::Internal(format!("the sandbox's network: {e:#}")))?;
            spec.network = Some(net);
        }
        let now = self.store.now()?;
        let sb = Sandbox { id, service: service.to_string(), environment: env.to_string(), image: spec.image.clone(), cpu, memory_mb, variables, network: network.to_string(), created_at: now, expires_at: now + ttl as i64 * 1000 };
        let (row, n) = (sb.clone(), name.clone());
        let added = self.db(move |s| s.add_sandbox(&row, &n, MAX_PER_ENV)).await;
        if !matches!(added, Ok(true)) {
            self.deps.runtime.remove_network(&network_name(&name)).await;
            added?;
            return Err(Error::Conflict(format!("{env} has {MAX_PER_ENV} sandboxes: delete one first")));
        }
        if let Err(e) = live.start_sandbox(&tpl, name.clone()).await {
            self.deps.runtime.remove_network(&network_name(&name)).await;
            let id = sb.id.clone();
            let _ = self.db(move |s| s.delete_sandbox(&id)).await;
            return Err(Error::Conflict(format!("the sandbox didn't start: {e:#}")));
        }
        Ok(sb)
    }

    pub async fn sandboxes(self: &Arc<Self>, service: &str, env: &str) -> Result<Vec<Sandbox>> {
        let (s, e) = (service.to_string(), env.to_string());
        Ok(self.db(move |st| st.sandboxes(Some(&s), Some(&e), None)).await?.into_iter().map(|(sb, _)| sb).collect())
    }

    async fn sandbox_row(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<(Sandbox, String)> {
        let (s, e) = (service.to_string(), env.to_string());
        self.db(move |st| st.sandboxes(Some(&s), Some(&e), None)).await?.into_iter().find(|(sb, _)| sb.id == id).ok_or_else(|| Error::NotFound(format!("no sandbox {id} in {env}")))
    }

    pub async fn sandbox(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<Sandbox> {
        Ok(self.sandbox_row(service, env, id).await?.0)
    }

    /// A sandbox stopped and gone, metered for as long as it ran.
    pub async fn delete_sandbox(self: &Arc<Self>, service: &str, env: &str, id: &str) -> Result<()> {
        let (sb, name) = self.sandbox_row(service, env, id).await?;
        self.end_sandbox(&sb, &name).await;
        Ok(())
    }

    async fn end_sandbox(self: &Arc<Self>, sb: &Sandbox, name: &str) {
        let _ = self.deps.runtime.stop(name, Duration::from_secs(2)).await;
        self.deps.runtime.remove(name).await;
        self.deps.runtime.remove_network(&network_name(name)).await;
        let id = sb.id.clone();
        if let Ok(true) = self.db(move |s| s.delete_sandbox(&id)).await {
            let now = code_sql::now_ms();
            let secs = ((now.min(sb.expires_at) - sb.created_at).max(0) as f64) / 1000.0;
            self.live(&sb.service, &sb.environment).meter.lock().sample(now, secs * sb.cpu * 0.5, (sb.memory_mb as u64) << 20, secs, 1);
        }
    }

    /// `POST …/sandboxes/{id}/exec`: `sh -c <command>` in it, its exit code and output (each
    /// stream to [`MAX_OUTPUT`], as text; a timeout kills it).
    pub async fn sandbox_exec(self: &Arc<Self>, service: &str, env: &str, id: &str, body: Value) -> Result<Value> {
        let command = body.get("command").and_then(Value::as_str).filter(|c| !c.trim().is_empty() && c.len() <= 64 << 10).ok_or_else(|| Error::BadRequest("command is a shell command (at most 64 KiB)".into()))?;
        let timeout = match body.get("timeout_s") {
            None => 60,
            Some(v) => v.as_u64().filter(|t| (1..=600).contains(t)).ok_or_else(|| Error::BadRequest("timeout_s is 1–600".into()))?,
        };
        let (sb, name) = self.sandbox_row(service, env, id).await?;
        if sb.expires_at <= code_sql::now_ms() {
            return Err(Error::Conflict(format!("{id} has ended")));
        }
        let out = self.deps.runtime.exec_output(&name, command, Duration::from_secs(timeout), MAX_OUTPUT).await.map_err(|e| Error::Conflict(format!("the command didn't run: {e:#}")))?;
        Ok(json!({ "exit_code": out.code, "timed_out": out.code.is_none(), "stdout": String::from_utf8_lossy(&out.stdout), "stderr": String::from_utf8_lossy(&out.stderr), "truncated": out.truncated }))
    }

    /// `PUT …/sandboxes/{id}/files?path=…`: the body written at `path` (an absolute path, at most
    /// [`MAX_FILE`]), its directory made.
    pub async fn sandbox_put_file(self: &Arc<Self>, service: &str, env: &str, id: &str, path: &str, data: bytes::Bytes) -> Result<()> {
        let (dir, file) = split_path(path)?;
        if data.len() > MAX_FILE {
            return Err(Error::BadRequest(format!("a file is at most {} MiB", MAX_FILE >> 20)));
        }
        let (_, name) = self.sandbox_row(service, env, id).await?;
        let mkdir = format!("mkdir -p '{}'", dir.replace('\'', "'\\''"));
        self.deps.runtime.exec_output(&name, &mkdir, Duration::from_secs(10), 4096).await.map_err(|e| Error::Conflict(format!("{e:#}")))?;
        let mut tar = tar::Builder::new(Vec::new());
        let mut head = tar::Header::new_gnu();
        head.set_size(data.len() as u64);
        head.set_mode(0o644);
        head.set_cksum();
        tar.append_data(&mut head, file, &data[..]).map_err(|e| Error::Internal(e.to_string()))?;
        let bytes = tar.into_inner().map_err(|e| Error::Internal(e.to_string()))?;
        self.deps.runtime.copy_to(&name, dir, Box::new(std::io::Cursor::new(bytes))).await.map_err(|e| Error::Conflict(format!("the file wasn't written: {e:#}")))
    }

    /// `GET …/sandboxes/{id}/files?path=…`: the file at `path` (at most [`MAX_FILE`]).
    pub async fn sandbox_get_file(self: &Arc<Self>, service: &str, env: &str, id: &str, path: &str) -> Result<Vec<u8>> {
        use tokio::io::AsyncReadExt;
        split_path(path)?;
        let (_, name) = self.sandbox_row(service, env, id).await?;
        let mut stream = self.deps.runtime.copy_from(&name, path).await.map_err(|e| Error::NotFound(format!("{path}: {e:#}")))?;
        // A tar of one entry: read to a bound past the file's own limit, then the first file.
        let mut raw = Vec::new();
        (&mut stream).take((MAX_FILE + (64 << 10)) as u64).read_to_end(&mut raw).await.map_err(|e| Error::Internal(e.to_string()))?;
        let mut ar = tar::Archive::new(&raw[..]);
        let mut entries = ar.entries().map_err(|e| Error::Internal(e.to_string()))?;
        let mut e = entries.next().ok_or_else(|| Error::NotFound(format!("no {path}")))?.map_err(|e| Error::Internal(e.to_string()))?;
        if !e.header().entry_type().is_file() {
            return Err(Error::BadRequest(format!("{path} isn't a file")));
        }
        if e.size() as usize > MAX_FILE {
            return Err(Error::BadRequest(format!("{path} is more than {} MiB", MAX_FILE >> 20)));
        }
        let mut out = Vec::new();
        std::io::Read::read_to_end(&mut e, &mut out).map_err(|e| Error::Internal(e.to_string()))?;
        Ok(out)
    }

    /// The owned services' sandboxes past their end, removed (the owner's minute sweep).
    pub(crate) async fn reap_sandboxes(self: &Arc<Self>, now: i64) {
        let Ok(all) = self.db(|s| s.sandboxes(None, None, None)).await else { return };
        // Past its end, or its service capped or its environment paused (refused at creation,
        // and stopped once that happens, as instances are).
        for (sb, name) in all.into_iter().filter(|(sb, _)| self.owns(&sb.service)) {
            if sb.expires_at <= now || self.capped.lock().contains(&sb.service) || self.live(&sb.service, &sb.environment).paused() {
                self.end_sandbox(&sb, &name).await;
            }
        }
    }
}

impl Apps {
    /// An environment's (every environment's with None) sandboxes gone with it: their containers
    /// went with its labels; their networks and rows go here, before the environment's rows.
    pub(crate) async fn drop_sandboxes(self: &Arc<Self>, service: &str, env: Option<&str>) {
        let (s, e) = (service.to_string(), env.map(str::to_string));
        let Ok(rows) = self.db(move |st| st.sandboxes(Some(&s), e.as_deref(), None)).await else { return };
        for (sb, name) in rows {
            self.deps.runtime.remove(&name).await;
            self.deps.runtime.remove_network(&network_name(&name)).await;
            let id = sb.id.clone();
            let _ = self.db(move |st| st.delete_sandbox(&id)).await;
        }
    }

    /// The containers of a service's sandboxes: what a takeover keeps (they end at their TTL).
    pub(crate) async fn sandbox_names(self: &Arc<Self>, service: &str) -> Vec<String> {
        let s = service.to_string();
        self.db(move |st| st.sandboxes(Some(&s), None, None)).await.map(|r| r.into_iter().map(|(_, n)| n).collect()).unwrap_or_default()
    }
}

/// A file's absolute path split into its directory and name (no `..`, no NUL).
fn split_path(path: &str) -> Result<(&str, &str)> {
    let bad = || Error::BadRequest("path is an absolute file path (/tmp/input.json)".into());
    if !path.starts_with('/') || path.len() > 1024 || path.contains('\0') || path.split('/').any(|c| c == "..") {
        return Err(bad());
    }
    let (dir, file) = path.rsplit_once('/').ok_or_else(bad)?;
    if file.is_empty() {
        return Err(bad());
    }
    Ok((if dir.is_empty() { "/" } else { dir }, file))
}

/// A sandbox's container: `roll-apps-<service>-<env>-sbx-<id>` (never an instance's name: those
/// end in a number).
fn sandbox_name(service: &str, env: &str, id: &str) -> String {
    format!("{}{}-{env}-sbx-{}", crate::runtime::PREFIX, service.trim_start_matches("svc_"), id.trim_start_matches("sbx_"))
}

/// The network a sandbox has of its own (`network: "none"`).
fn network_name(container: &str) -> String {
    format!("{container}-n")
}
