//! Where a preview's services run: roll apps' catalog, else roll.toml containers.

use super::*;
use async_trait::async_trait;
use reqwest::Method;

use serde_json::{Value, json};
use std::sync::Arc;

// ── Services: roll apps' catalog when the project has one, else roll.toml containers ──

/// How long one converge waits for a roll apps build and rollout before coming back later.
const DEPLOY_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// roll apps (docs/apps/) over this node's API: a preview is an ephemeral environment
/// `pv-<id>` of each of the project's services, deployed at the change's commit. roll apps
/// injects the roll secrets environment of the same name; the branches' URLs are also set as
/// variables, for a project without roll secrets.
pub struct RollApps {
    api: Api,
    /// (preview, service) → service id, learned at deploy (looked up again after a restart).
    ids: std::sync::Mutex<std::collections::HashMap<(String, String), String>>,
    /// roll apps takes `feedback` on an environment (roll apps' #26; an older one refuses an
    /// unknown field): previews' button on its pages.
    /// The deployment last started per (preview, service), with what it deploys (`key`): a poll
    /// while it's under way waits on it, and once it's live nothing is started again.
    started: std::sync::Mutex<std::collections::HashMap<(String, String), (String, String)>>,
    pub feedback: bool,
}

impl RollApps {
    pub fn new(api: Api) -> RollApps {
        RollApps { api, ids: Default::default(), started: Default::default(), feedback: false }
    }


    /// The roll apps service `name` of `project`.
    async fn service_in(&self, project: &str, name: &str) -> anyhow::Result<Option<String>> {
        let list = self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await?;
        Ok(list.as_array().into_iter().flatten().find(|x| x["name"] == name).and_then(|x| s(&x["id"])))
    }

    /// The service of `preview` named `name`: one with an environment `pv-<id>`. `Ok(None)` only
    /// when roll apps answered and has none (or roll apps isn't on this node): an error isn't "gone".
    async fn find(&self, preview: &str, name: &str) -> anyhow::Result<Option<(String, String)>> {
        let env = roll_previews::engine::env_name(preview);
        if let Some(id) = self.ids.lock().unwrap_or_else(|p| p.into_inner()).get(&(preview.to_string(), name.to_string())) {
            return Ok(Some((id.clone(), env)));
        }
        let all = match self.api.get("/v1/apps/services").await {
            Ok(a) => a,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let found = all
            .as_array()
            .into_iter()
            .flatten()
            .find(|x| x["name"] == name && x["environments"].as_array().is_some_and(|e| e.iter().any(|e| e["name"] == env.as_str())))
            .and_then(|svc| s(&svc["id"]));
        let Some(id) = found else { return Ok(None) };
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((preview.to_string(), name.to_string()), id.clone());
        Ok(Some((id, env)))
    }

    /// Wait a little for a deployment roll apps accepted (202: queued) to finish: `Err` if it
    /// failed or was canceled (the preview says so and retries), `StillDeploying` if it's still
    /// going after [`DEPLOY_WAIT`] (the converge comes back; nothing holds a slot for a build).
    async fn settle(&self, path: &str, mut d: Value) -> anyhow::Result<()> {
        let deadline = std::time::Instant::now() + DEPLOY_WAIT;
        loop {
            let status = d["status"].as_str().unwrap_or("");
            match status {
                "queued" | "building" | "deploying" => {}
                "failed" | "canceled" => anyhow::bail!("roll apps deploy {} {status}: {}", s(&d["id"]).unwrap_or_default(), s(&d["error"]).unwrap_or_else(|| "no reason given".into())),
                // active, or already replaced by a newer one (superseded): done.
                _ => return Ok(()),
            }
            if std::time::Instant::now() >= deadline {
                return Err(roll_previews::ports::StillDeploying(format!("roll apps deploy {} is {status}", s(&d["id"]).unwrap_or_default())).into());
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let id = s(&d["id"]).ok_or_else(|| anyhow::anyhow!("a roll apps deployment without an id"))?;
            d = self.api.get(&format!("{path}/deploys/{}", seg(&id))).await?;
        }
    }

    /// Every service's view of the preview's environment, as roll apps has them (not only those
    /// this process deployed: after a restart a preview that doesn't redeploy still says).
    async fn pv_environments(&self, preview: &str) -> Vec<Value> {
        let env = roll_previews::engine::env_name(preview);
        let all = self.api.get("/v1/apps/services").await.unwrap_or(Value::Null);
        let ids: Vec<String> = all
            .as_array()
            .into_iter()
            .flatten()
            .filter(|x| x["environments"].as_array().is_some_and(|e| e.iter().any(|e| e["name"] == env.as_str())))
            .filter_map(|x| s(&x["id"]))
            .collect();
        let mut out = Vec::new();
        for id in ids {
            if let Ok(e) = self.api.get(&Self::env_path(&id, &env)).await {
                out.push(e);
            }
        }
        out
    }

    fn env_path(id: &str, env: &str) -> String {
        format!("/v1/apps/services/{}/environments/{}", seg(id), seg(env))
    }
}

#[async_trait]
impl roll_previews::ports::Apps for RollApps {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        let list = match self.api.get(&format!("/v1/apps/projects/{}/services", seg(project))).await {
            Ok(l) => l,
            // No roll apps on this node (or the project has none): roll.toml's services.
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        Ok(Some(
            list.as_array()
                .into_iter()
                .flatten()
                // An upload source deploys only what someone uploads: nothing for a preview to build.
                .filter(|x| x["source"]["kind"] != "upload")
                .map(|x| roll_previews::ports::CatalogService {
                    name: s(&x["name"]).unwrap_or_default(),
                    // A Git URL service is `git:<its id>`: its branch previews are made by roll apps'
                    // pushes (`Engine::git_push`), at the pushed commit.
                    repo: match x["source"]["kind"].as_str() {
                        Some("repo") => s(&x["source"]["repo"]),
                        Some("git") => s(&x["id"]).map(|id| format!("git:{id}")),
                        _ => None,
                    },
                    root: s(&x["source"]["root"]).unwrap_or_default(),
                    memory_mb: x["run"]["memory_mb"].as_u64().unwrap_or(512) as u32,
                })
                .collect(),
        ))
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        let id = self.service_in(&t.project, &t.service).await?.ok_or_else(|| anyhow::anyhow!("roll apps has no service {} in the project", t.service))?;
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).insert((t.preview.clone(), t.service.clone()), id.clone());
        let path = Self::env_path(&id, &t.environment);
        // Private like the preview: roll apps' proxy checks the same token (link access), or
        // signs people in through roll's dashboard (team access); the bypass opens either.
        let access = match (&t.access_token, t.signed_in) {
            (Some(token), _) => Some(json!({ "token": token })),
            (None, true) => Some(json!({ "signed_in": true })),
            (None, false) => None,
        }
        .map(|mut a| {
            if let Some(hash) = &t.bypass_sha256 {
                a["bypass_sha256"] = json!(hash);
            }
            a
        });
        // The project's egress: with `roll`, roll apps runs it on a network with no route out (a
        // runtime that can't is a 409, which fails the deploy rather than running it open).
        let closed = t.egress == roll_previews::settings::EgressRule::Roll;
        let egress = if closed { json!({ "mode": "roll", "hosts": t.egress_hosts }) } else { json!({ "mode": "open" }) };
        // One voice on a pull request: previews posts `roll/preview` and its comment for the
        // environment, so roll apps posts no commit status or GitHub Deployment of its own (apps' #249).
        let mut body = json!({ "branch": null, "ephemeral": true, "access": access, "egress": egress, "report": false });
        // Its schedule (a cron's, a service's scheduled commands and calls) runs in a preview only
        // when the project says so: deployed and runnable on demand either way (roll apps'
        // `scheduled`).
        body["scheduled"] = json!(t.crons);
        // Who its keys bound to hosts are used as (roll apps resolves them): the rules' maker, or
        // nobody (withheld), never whoever made the call (the operator: rolld in process).
        if closed {
            body["closed_by"] = json!(t.maker);
        }
        // The project's traces: roll apps keeps the environment's itself (its own OpenTelemetry
        // variables), and the preview's sheet reads them from it.
        // Off is said too: roll apps keeps an environment's `traces` as it was when a field is left out.
        body["traces"] = json!(t.traces);
        if self.feedback {
            body["feedback"] = json!(t.feedback);
        }
        // Its label under the project's own preview domain (`<label>.preview.acme.dev`, roll apps'
        // project preview domain): apps routes that name to this environment.
        body["preview_host"] = json!(t.host);
        // A roll apps that predates a field refuses it (a node not yet upgraded, on a fleet):
        // `report` (#249; then its deploys report as well as previews), `preview_host` (its
        // rolld preview URL still works), `closed_by` (its maker is who set the egress, as
        // before) or `scheduled` (a preview's cron runs its schedule, as before). The environment
        // is made without the fields it doesn't know.
        let mut made = self.api.call(Method::PUT, &path, Some(body.clone())).await;
        for _ in 0..4 {
            let unknown = match &made {
                Err(e) => e.downcast_ref::<super::Refused>().filter(|r| r.status.as_u16() == 400).and_then(|r| r.error.strip_prefix("unknown field ")).filter(|f| matches!(*f, "report" | "preview_host" | "closed_by" | "scheduled")).map(str::to_string),
                Ok(_) => None,
            };
            let Some(field) = unknown else { break };
            body.as_object_mut().map(|o| o.remove(&field));
            made = self.api.call(Method::PUT, &path, Some(body.clone())).await;
        }
        let made = made?;
        // Roll only holds the build too: roll apps builds a closed environment without its secret
        // values (apps' #222, `build_closed`). One that doesn't say so would build with them on an
        // open network, where a fork's head could send them out: nothing is deployed there.
        if closed && made["build_closed"] != json!(true) {
            anyhow::bail!("roll apps here doesn't build closed environments without their secrets (build_closed): under Roll only, nothing is deployed to it");
        }
        // Every variable the preview sets: the branches' plain names and the project's overrides,
        // and each prefixed link to a branched parent pointed at the branch (a link is roll apps'
        // own variable, which would still reach production).
        let mut set: serde_json::Map<String, Value> = t.vars.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let links = self.api.get(&format!("/v1/apps/services/{}/links", seg(&id))).await.unwrap_or(Value::Null);
        for l in links.as_array().into_iter().flatten() {
            let Some((_, _, vars)) = t.branches.iter().find(|(k, parent, _)| l["kind"] == k.as_str() && l["ref"] == parent.as_str()) else { continue };
            let prefix = l["prefix"].as_str().unwrap_or("");
            // Every variable the link gives, from the branch: by roll apps' names, and blank for
            // one the branch has no value for, never production's (a stream's brokers and login,
            // an OpenSearch login, would reach production's data).
            for name in l["vars"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let plain = name.strip_prefix(prefix).unwrap_or(name);
                set.insert(name.to_string(), json!(link_value(plain, vars).unwrap_or_default()));
            }
        }
        // What roll set last time and no longer wants (a removed preview variable or project
        // override) goes; names a person or the base environment set are never touched. The names
        // roll set are kept in the environment itself, so a restart knows them.
        let before = self.api.get(&format!("{path}/variables/values")).await.unwrap_or(Value::Null);
        self.api.call(Method::PATCH, &format!("{path}/variables"), Some(variables_patch(&before, set.clone()))).await?;
        // At the sha already (a reset's new URLs, say): run it again with the variables, no build.
        // Otherwise deploy, keyed so a retry after a crash returns the same deployment.
        let env = self.api.get(&path).await?;
        // What's deployed, named: the commit and what it's deployed with.
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        let mut sorted: Vec<_> = set.iter().collect();
        sorted.sort_by(|a, b| a.0.cmp(b.0));
        for (k, v) in sorted {
            for b in k.bytes().chain([0]).chain(v.to_string().into_bytes()).chain([0]) {
                h = (h ^ b as u64).wrapping_mul(0x0100_0000_01b3);
            }
        }
        // What it may reach is part of what's deployed: a changed rule deploys again.
        let key = format!(
            "{}@{}@{h:016x}{}{}{}{}",
            t.environment,
            t.git.as_deref().unwrap_or("image"),
            if t.access_token.is_some() { "p" } else if t.signed_in { "s" } else { "" },
            if self.feedback && t.feedback { "f" } else { "" },
            (if closed { format!("e{}{}", t.egress_hosts.join(","), t.maker.as_deref().map(|m| format!("m{m}")).unwrap_or_default()) } else { String::new() }) + if t.crons { "k" } else { "" },
            t.rebuild.map(|n| format!("#r{n}")).unwrap_or_default()
        );
        // The deployment previews last started for exactly this, still under way or live: wait on
        // it (or it's done). The engine asks again while a deploy is in flight, and a redeploy
        // that starts another supersedes it each time.
        let mine = (t.preview.clone(), t.service.clone());
        let last = self.started.lock().unwrap_or_else(|p| p.into_inner()).get(&mine).cloned();
        match already(last.as_ref(), &key, &env) {
            Already::UnderWay => {
                self.settle(&path, env["latest"].clone()).await?;
                return Ok(s(&self.api.get(&path).await?["url"]));
            }
            Already::Live => return Ok(s(&env["url"])),
            Already::No => {}
        }
        // At the sha already (a reset's new URLs, say): run it again with the variables, no build.
        // Otherwise deploy, keyed so a retry after a crash returns the same deployment.
        // A deploy hook's rebuild builds again even at the sha.
        let at_sha = t.rebuild.is_none() && t.git.is_some() && env["active"]["revision"].as_str() == t.git.as_deref();
        let deployment = if at_sha {
            // Keyed too (roll apps takes `Idempotency-Key` on a redeploy): what's deployed and the
            // deployment it replaces, so a retry after a crash gets the same one, and variables
            // that come back to an earlier set never get that earlier, superseded, redeploy back.
            let idem = redeploy_key(&key, &s(&env["active"]["id"]).unwrap_or_default());
            let mut body = json!({ "message": format!("roll preview {}: new variables", t.preview) });
            if closed {
                body["closed"] = json!(true);
            }
            self.api.call_with(Method::POST, &format!("{path}/redeploy"), Some(body), Some(&idem)).await?
        } else {
            let mut body = match &t.git {
                Some(sha) => json!({ "revision": sha, "message": format!("roll preview {}", t.preview) }),
                None => json!({ "message": format!("roll preview {}", t.preview) }),
            };
            // Nothing cached: the build's steps run again (a static site's content fetched anew).
            if t.rebuild.is_some() {
                body["no_cache"] = json!(true);
                body["message"] = json!(format!("roll preview {}: built again by a deploy hook", t.preview));
            }

            // The build itself must be closed (the node that takes the deploy says so, or refuses
            // it: one from before the field answers 400), not only the environment's answer above
            // (a fleet's rolling upgrade can serve the two on different nodes).
            if closed {
                body["closed"] = json!(true);
            }
            let mut idem = key.clone();
            // After a failed or canceled deploy, a retry is a new deploy (a registry timeout
            // doesn't fail the preview until the next push); a crash's retry still finds its own.
            if matches!(env["latest"]["status"].as_str(), Some("failed" | "canceled"))
                && let Some(failed) = s(&env["latest"]["id"])
            {
                idem.push_str(&format!("~{failed}"));
            }
            self.api.call_with(Method::POST, &format!("{path}/deploys"), Some(body), Some(&idem)).await?
        };
        if let Some(id) = s(&deployment["id"]) {
            self.started.lock().unwrap_or_else(|p| p.into_inner()).insert(mine, (id, key));
        }
        self.settle(&path, deployment).await?;
        let env = self.api.get(&path).await?;
        Ok(s(&env["url"]))
    }

    /// The project's verified preview domain (`GET /v1/apps/projects/{project}/preview-domain`:
    /// `{hostname: "*.preview.acme.dev", status, tls, …}`; 404 when it has none).
    async fn preview_domain(&self, project: &str) -> Option<(String, bool)> {
        let d = self.api.get(&format!("/v1/apps/projects/{}/preview-domain", seg(project))).await.ok()?;
        (d["status"].as_str() == Some("active")).then_some(())?;
        let base = d["hostname"].as_str()?.strip_prefix("*.")?.to_ascii_lowercase();
        Some((base, d["tls"].as_str() == Some("issued")))
    }

    async fn memory_mb(&self, preview: &str, service: &str) -> Option<u32> {
        // The live deployment's frozen run: `[app.previews.run]` and the environment's own size
        // over the service's.
        let (id, env) = self.find(preview, service).await.ok().flatten()?;
        let e = self.api.get(&Self::env_path(&id, &env)).await.ok()?;
        e["active"]["run"]["memory_mb"].as_u64().map(|m| m as u32)
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        let (id, env) = self.find(preview, service).await?.ok_or_else(|| anyhow::anyhow!("roll apps has no preview environment for {service}"))?;
        self.api.post(&format!("{}/wake", Self::env_path(&id, &env)), json!({})).await.map(|_| ())
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if let Ok(Some((id, env))) = self.find(preview, service).await {
            let _ = self.api.post(&format!("{}/sleep", Self::env_path(&id, &env)), json!({})).await;
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        if let Some((id, env)) = self.find(preview, service).await? {
            // Gone already counts as done; anything else is retried.
            self.api.delete(&Self::env_path(&id, &env)).await?;
        }
        self.ids.lock().unwrap_or_else(|p| p.into_inner()).remove(&(preview.to_string(), service.to_string()));
        self.started.lock().unwrap_or_else(|p| p.into_inner()).remove(&(preview.to_string(), service.to_string()));
        Ok(())
    }

    /// The `pv-` environment's live deployment: its git commit and its own URL (roll apps'
    /// deployment URL; none for a worker, a service with a volume, or a roll apps without `url`).
    async fn live_build(&self, preview: &str, service: &str) -> Option<roll_previews::ports::LiveBuild> {
        let (id, env) = self.find(preview, service).await.ok()??;
        let path = Self::env_path(&id, &env);
        let dep = s(&self.api.get(&path).await.ok()?["active"]["id"])?;
        let d = self.api.get(&format!("{path}/deploys/{}", seg(&dep))).await.ok()?;
        Some(roll_previews::ports::LiveBuild { id: dep, revision: s(&d["revision"])?, url: s(&d["url"])? })
    }

    /// The `pv-` environment's `check_states`: production's browser checks, run once on each of
    /// its heads; a result counts when it's of the live deployment (`deployment`).
    async fn browser_checks(&self, preview: &str, service: &str) -> Option<Vec<roll_previews::ports::BrowserCheckResult>> {
        let (id, env) = self.find(preview, service).await.ok()??;
        let e = self.api.get(&Self::env_path(&id, &env)).await.ok()?;
        let live = e["active"]["id"].as_str()?.to_string();
        let states = e["check_states"].as_array().filter(|a| !a.is_empty())?;
        Some(
            states
                .iter()
                .filter_map(|c| {
                    let name = s(&c["name"])?;
                    let ran = c["deployment"].as_str() == Some(live.as_str());
                    let passed = if ran { c["up"].as_bool() } else { None };
                    // A failure's screenshot and trace, kept by roll apps under this environment.
                    let files = (passed == Some(false) && c["failure_at"].is_number()).then(|| roll_previews::BrowserCheckFiles { service_id: id.clone(), environment: env.clone() });
                    Some(roll_previews::ports::BrowserCheckResult { name, passed, error: s(&c["error"]).filter(|_| passed == Some(false)), files })
                })
                .collect(),
        )
    }

    /// roll apps' rollback of `deployment` taking no traffic, with the copy's variables in place
    /// of the ones roll set (`plain`, and each link to a branched parent from `branches`, as a
    /// deploy sets them; only names the environment has, which roll apps requires), polled until
    /// it's ready (a minute at most): its own URL.
    async fn view_build(&self, preview: &str, service: &str, deployment: &str, plain: &[(String, String)], branches: &[roll_previews::ports::BranchVars]) -> anyhow::Result<Option<String>> {
        let Some((id, env)) = self.find(preview, service).await? else { return Ok(None) };
        let path = Self::env_path(&id, &env);
        let mut set: serde_json::Map<String, Value> = plain.iter().map(|(k, v)| (k.clone(), json!(v))).collect();
        let links = self.api.get(&format!("/v1/apps/services/{}/links", seg(&id))).await.unwrap_or(Value::Null);
        for l in links.as_array().into_iter().flatten() {
            let Some((_, _, vars)) = branches.iter().find(|(k, parent, _)| l["kind"] == k.as_str() && l["ref"] == parent.as_str()) else { continue };
            let prefix = l["prefix"].as_str().unwrap_or("");
            for name in l["vars"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let plain = name.strip_prefix(prefix).unwrap_or(name);
                set.insert(name.to_string(), json!(link_value(plain, vars).unwrap_or_default()));
            }
        }
        let has = self.api.get(&format!("{path}/variables/values")).await?;
        set.retain(|k, _| has.get(k).is_some());
        anyhow::ensure!(!set.is_empty(), "its environment has none of the copy's variables");
        let made = self.api.post(&format!("{path}/deploys/{}/rollback", seg(deployment)), json!({ "no_traffic": true, "variables": set })).await?;
        let dep = s(&made["id"]).ok_or_else(|| anyhow::anyhow!("a rollback without an id"))?;
        for _ in 0..60 {
            let d = self.api.get(&format!("{path}/deploys/{}", seg(&dep))).await?;
            match d["status"].as_str() {
                Some("ready" | "active") => return Ok(s(&d["url"])),
                Some(st @ ("failed" | "canceled" | "superseded")) => anyhow::bail!("its build on the copy {st}: {}", d["error"].as_str().unwrap_or("")),
                _ => tokio::time::sleep(std::time::Duration::from_secs(1)).await,
            }
        }
        anyhow::bail!("its build on the copy wasn't ready within a minute")
    }

    /// The `pv-` environment's latest minute (roll apps' metrics): cores, and resident bytes.
    /// None while nothing ran that minute (asleep).
    async fn metrics(&self, preview: &str, service: &str) -> Option<(f64, u64)> {
        let (id, env) = self.find(preview, service).await.ok()??;
        let m = self.api.get(&format!("{}/metrics?minutes=5", Self::env_path(&id, &env))).await.ok()?;
        // The last finished minute: the one under way counts its CPU over the whole minute, so it
        // reads low (and its memory is 0 in its first second). Memory is summed over instances, and
        // shown against what one runs with: one instance's, on average.
        let at = |k: &str| m[k].as_array().and_then(|a| a.get(a.len().saturating_sub(2)).cloned());
        let (cpu, mem) = (at("cpu")?.as_f64()?, at("memory_bytes")?.as_u64()?);
        let instances = at("instances").and_then(|v| v.as_u64()).unwrap_or(1).max(1);
        (mem > 0).then_some((cpu, mem / instances))
    }

    /// The `pv-` environment's errors, as roll apps groups them (its 50 most frequent; the engine
    /// masks them).
    async fn errors(&self, preview: &str, service: &str, minutes: i64, _masks: Option<&roll_previews::engine::ErrorMasks<'_>>) -> roll_previews::Result<Vec<roll_previews::ErrorGroup>> {
        let Ok(Some((id, env))) = self.find(preview, service).await else { return Ok(Vec::new()) };
        let Ok(v) = self.api.get(&format!("{}/errors?minutes={minutes}", Self::env_path(&id, &env))).await else { return Ok(Vec::new()) };
        Ok(serde_json::from_value(v["groups"].clone()).unwrap_or_default())
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        let Ok(Some((id, env))) = self.find(preview, service).await else { return Vec::new() };
        let v = self.api.get(&format!("{}/logs?tail={lines}", Self::env_path(&id, &env))).await.unwrap_or(Value::Null);
        v.as_array().into_iter().flatten().filter_map(|l| s(&l["text"])).collect()
    }

    async fn traces(&self, preview: &str, service: &str, minutes: i64) -> Vec<roll_previews::engine::traces::TraceRow> {
        let Ok(Some((id, env))) = self.find(preview, service).await else { return Vec::new() };
        let Ok(v) = self.api.get(&format!("{}/traces?minutes={minutes}", Self::env_path(&id, &env))).await else { return Vec::new() };
        serde_json::from_value(v["traces"].clone()).unwrap_or_default()
    }

    async fn trace(&self, preview: &str, service: &str, trace: &str) -> Option<Vec<roll_previews::engine::traces::Span>> {
        let (id, env) = self.find(preview, service).await.ok()??;
        let v = self.api.get(&format!("{}/traces/{trace}", Self::env_path(&id, &env))).await.ok()?;
        serde_json::from_value(v["spans"].clone()).ok()
    }

    fn status(&self, _preview: &str, _service: &str) -> Option<roll_previews::ServiceState> {
        None
    }

    // roll apps' instances don't freeze.
    fn warm(&self, _preview: &str, _service: &str) -> bool {
        false
    }

    /// The preview's environments' interception (apps' `intercepted`, per closed network since
    /// its proxy came up), added up; the latest refusals, each once.
    async fn intercepted(&self, preview: &str) -> Option<roll_previews::AppsIntercepted> {
        let mut out: Option<roll_previews::AppsIntercepted> = None;
        for e in self.pv_environments(preview).await {
            let i = &e["intercepted"];
            if !i.is_object() {
                continue;
            }
            let x = out.get_or_insert_with(Default::default);
            x.requests += i["requests"].as_u64().unwrap_or(0);
            x.swapped += i["swapped"].as_u64().unwrap_or(0);
            for r in i["refused"].as_array().into_iter().flatten().filter_map(s) {
                if !x.refused.contains(&r) {
                    x.refused.push(r);
                }
            }
        }
        out
    }

    /// What the preview's environments' egress proxies refused, each host once: `<host>`, or
    /// `<host> private` for a listed one that resolves only to private addresses.
    async fn egress_refused(&self, preview: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for e in self.pv_environments(preview).await {
            for r in e["egress_refused"].as_array().into_iter().flatten() {
                let Some(host) = s(&r["host"]) else { continue };
                let line = if r["private"] == true { format!("{host} private") } else { host };
                if !out.contains(&line) {
                    out.push(line);
                }
            }
        }
        out
    }
}

/// A roll apps link's variable `name` (its prefix taken off) as a preview's branch gives it:
/// the branch's own variable of that name, or roll apps' name for one the branch names otherwise
/// (a bucket's endpoint, a stream's brokers and SASL login, a search cluster's API key or
/// OpenSearch login, from the branch's `ELASTICSEARCH_URL` and its credentials). `None`: the
/// branch has nothing for it.
fn link_value(name: &str, vars: &[(String, String)]) -> Option<String> {
    let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let search = || get("ELASTICSEARCH_URL").and_then(|u| url::Url::parse(&u).ok());
    // With the branch's data-only credential (as production's link has), the URL carries none
    // of the branch's own login: the key, or the data user, is what the service gets.
    match name {
        "ELASTICSEARCH_URL" if get("ELASTICSEARCH_API_KEY").is_some() => {
            return search().map(|mut u| {
                let _ = (u.set_username(""), u.set_password(None));
                u.to_string().trim_end_matches('/').to_string()
            });
        }
        "OPENSEARCH_URL" => {
            if let (Some(user), Some(pass)) = (get("OPENSEARCH_USERNAME"), get("OPENSEARCH_PASSWORD")) {
                return search().map(|mut u| {
                    let _ = (u.set_username(&user), u.set_password(Some(&pass)));
                    u.to_string().trim_end_matches('/').to_string()
                });
            }
        }
        _ => {}
    }
    if let Some(v) = get(name) {
        return Some(v);
    }
    let decode = |s: &str| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
    match name {
        "AWS_ENDPOINT_URL" => get("AWS_ENDPOINT_URL_S3"),
        "KAFKA_BROKERS" => get("KAFKA_BOOTSTRAP_SERVERS"),
        "KAFKA_USERNAME" => get("KAFKA_SASL_USERNAME"),
        "KAFKA_PASSWORD" => get("KAFKA_SASL_PASSWORD"),
        "OPENSEARCH_URL" => get("ELASTICSEARCH_URL"),
        "OPENSEARCH_USERNAME" => search().map(|u| decode(u.username())).filter(|u| !u.is_empty()),
        "OPENSEARCH_PASSWORD" => search().and_then(|u| u.password().map(decode)),
        // roll search takes `ApiKey base64(<cluster>:<password>)` for a cluster's own login;
        // a `<user>@<cluster>` login has no key form.
        "ELASTICSEARCH_API_KEY" => search().and_then(|u| {
            use base64::Engine as _;
            let (user, pass) = (decode(u.username()), u.password().map(decode)?);
            (!user.is_empty() && !user.contains('@')).then(|| base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}")))
        }),
        _ => None,
    }
}

/// The variable in a preview's roll apps environment that names the variables roll set there.
const SET_BY_ROLL: &str = "ROLL_PREVIEW_SET";

/// roll apps' variables `PATCH` for a preview environment whose values are `before`: `set`, the
/// names roll set last time (its `SET_BY_ROLL`) and no longer wants unset, and the new list.
/// Every value roll sets is sealed (the branches' credentials, the project's overrides, the
/// preview's own variables): roll apps hides sealed values in its logs and never returns them
/// (previews' own `…/env` is where they're revealed). Sealing is one-way, so sending it again is
/// harmless.
fn variables_patch(before: &Value, mut set: serde_json::Map<String, Value>) -> Value {
    let prev = before[SET_BY_ROLL].as_str().unwrap_or("");
    let unset: Vec<&str> = prev.split(',').map(str::trim).filter(|n| !n.is_empty() && *n != SET_BY_ROLL && !set.contains_key(*n)).collect();
    let names: Vec<String> = set.keys().cloned().collect();
    set.insert(SET_BY_ROLL.into(), json!(names.join(",")));
    json!({ "set": set, "unset": unset, "seal": names })
}


/// roll apps for the services in its catalog, the container runtime for `roll.toml`'s.
pub struct Services {
    pub apps: RollApps,
    /// roll apps' own listener (`--apps-listen`): a roll apps service's page read through roll
    /// (`…/fetch`) goes in there, by its host, as a visitor's request does.
    pub apps_listen: std::net::SocketAddr,
    pub runtime: Arc<roll_previews::runtime::Runtime>,
    /// Screenshots of roll apps' instances that run on this node (`None`: no roll apps here, so a
    /// project on roll apps gets no screenshots and no baseline).
    pub shots: Option<Arc<dyn super::fleet::Shoot>>,
    /// On a fleet: an instance runs on its service's holder, where its screenshot is taken.
    pub fleet: Option<Arc<roll_cluster::Cluster>>,
}

/// A screenshot of a roll apps instance on this node: the same browser run as roll.toml's
/// services, in the instance's own network, held awake meanwhile.
pub struct AppsShots {
    pub reach: Arc<dyn Reach>,
    pub runtime: Arc<roll_previews::runtime::Runtime>,
}

#[async_trait]
impl super::fleet::Shoot for AppsShots {
    async fn shoot(&self, service: &str, env: &str, opts: &roll_previews::ports::ShotOptions) -> anyhow::Result<Option<roll_previews::ports::Shot>> {
        let Some(at) = self.reach.reach(service, env).await? else { return Ok(None) };
        let shot = self.runtime.screenshot_in(&at.container, at.port, opts).await;
        drop(at.hold);
        shot.map(Some)
    }
}

/// Where a roll apps instance can be screenshotted (roll apps' `Apps::reachable`): its container
/// on this host and the port the app listens on inside it, the instance woken and kept awake
/// while the answer lives. `None` where there's nothing to join (Fargate).
#[async_trait]
pub trait Reach: Send + Sync {
    async fn reach(&self, service: &str, env: &str) -> anyhow::Result<Option<Reached>>;
}

/// roll previews' notes for roll apps' feedback button, by the preview's environment name.
/// roll apps' pushes to branches of a Git URL service that none of its environments follows: each
/// branch's preview (`Engine::git_push`). Handed off at once, the hook is waiting.
/// A Git URL service's preview where its code lives (docs/previews.md, "Git URL services"):
/// through roll apps' GitHub App connection, `github: {installation, repo}` read from roll apps
/// (never from an event), with short-lived installation tokens (roll code's client). A service
/// that isn't connected gets nothing; its git login is never used to write.
pub struct GitReport {
    pub api: Api,
    pub code: std::sync::Arc<code_control::Code>,
}

impl GitReport {
    /// The service's connection: installation, owner, repository.
    async fn connection(&self, service: &str) -> anyhow::Result<Option<(u64, String, String)>> {
        let s = match self.api.get(&format!("/v1/apps/services/{}", seg(service))).await {
            Ok(s) => s,
            Err(e) if is_404(&e) => return Ok(None),
            Err(e) => return Err(e),
        };
        let g = &s["github"];
        let (Some(installation), Some((owner, repo))) = (g["installation"].as_u64(), g["repo"].as_str().and_then(|r| r.split_once('/'))) else { return Ok(None) };
        Ok(Some((installation, owner.to_string(), repo.to_string())))
    }
}

#[async_trait]
impl roll_previews::ports::GitReport for GitReport {
    async fn status(&self, service: &str, sha: &str, state: &str, description: &str, target_url: Option<&str>) -> anyhow::Result<bool> {
        let Some((installation, owner, repo)) = self.connection(service).await? else { return Ok(false) };
        self.code.github_commit_status(installation, &owner, &repo, sha, state, "roll/preview", description, target_url).await.map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(true)
    }

    async fn comment(&self, service: &str, pull_repo: &str, number: u64, body: &str, existing: Option<u64>) -> anyhow::Result<Option<u64>> {
        let Some((installation, owner, repo)) = self.connection(service).await? else { return Ok(None) };
        // Connected to another repository since (its source moved): this number isn't its.
        if !format!("{owner}/{repo}").eq_ignore_ascii_case(pull_repo) {
            return Ok(None);
        }
        self.code.github_comment(installation, &owner, &repo, number, body, existing).await.map(Some).map_err(|e| anyhow::anyhow!("{e}"))
    }
}

pub struct GitPushes(pub std::sync::Weak<roll_previews::Engine>);

#[async_trait]
impl apps_control::integrations::GitPushes for GitPushes {
    /// Pull requests too: previews names a same-repo one's branch preview, and runs a fork's only
    /// at a head a writer approved.
    fn takes_pulls(&self) -> bool {
        true
    }

    async fn pushed(&self, push: apps_control::integrations::GitPush) {
        let Some(engine) = self.0.upgrade() else { return };
        let pull = push.pull.map(|p| roll_previews::ports::GitPull { number: p.number, title: p.title, url: p.url, fork: p.fork, author: p.author });
        let p = roll_previews::ports::GitPush { service: push.service, url: push.url, branch: push.branch, sha: push.sha, message: push.message, pusher: push.pusher, pull };
        tokio::spawn(async move {
            if let Err(e) = engine.git_push(&p).await {
                tracing::warn!(service = %p.service, branch = %p.branch, error = %e, "previews: a Git URL service's push");
            }
        });
    }
}

/// Names under a project's own preview domain that no roll apps environment holds (a service on
/// roll's own runtime): roll apps' ingress hands them on, in process, to previews' router, which
/// answers a 404 for a name it doesn't hold.
pub struct DomainRouter(pub Arc<roll_previews::runtime::Runtime>);

#[async_trait]
impl apps_control::integrations::PreviewRouter for DomainRouter {
    async fn serve(&self, req: axum::extract::Request) -> axum::response::Response {
        roll_previews::runtime::serve_host(self.0.clone(), req).await
    }
}

pub struct Notes(pub std::sync::Weak<roll_previews::Engine>);

#[async_trait]
impl apps_control::integrations::FeedbackNotes for Notes {
    async fn notes(&self, env: &str, path: &str) -> anyhow::Result<Vec<apps_control::integrations::Note>> {
        let Some(engine) = self.0.upgrade() else { return Ok(Vec::new()) };
        Ok(engine
            .notes_for(env, path)
            .await?
            .into_iter()
            .map(|n| apps_control::integrations::Note { path: n.path, selector: n.selector, note: n.note, by: n.by, at: n.at })
            .collect())
    }
}

/// roll apps' own (`Apps::reachable`): the instance's container and port, its lease held.
/// roll apps' log drains, for previews' services on roll's runtime: whether a project's take
/// previews' output is read again after [`DRAINS_FOR`].
pub struct AppsDrains {
    pub apps: Arc<apps_control::Apps>,
    pub wanted: std::sync::Mutex<std::collections::HashMap<(String, roll_previews::ports::DrainSource), (std::time::Instant, bool)>>,
}

const DRAINS_FOR: std::time::Duration = std::time::Duration::from_secs(30);

#[async_trait]
impl roll_previews::ports::Drains for AppsDrains {
    async fn wanted(&self, project: &str, source: roll_previews::ports::DrainSource) -> bool {
        let key = (project.to_string(), source);
        if let Some((at, w)) = self.wanted.lock().unwrap_or_else(|e| e.into_inner()).get(&key)
            && at.elapsed() < DRAINS_FOR
        {
            return *w;
        }
        let w = self.apps.drains_take_previews(project, None, source.as_str()).await.unwrap_or(false);
        let mut m = self.wanted.lock().unwrap_or_else(|e| e.into_inner());
        if m.len() > 5000 {
            m.clear();
        }
        m.insert(key, (std::time::Instant::now(), w));
        w
    }

    fn send(&self, project: &str, environment: &str, service: &str, source: roll_previews::ports::DrainSource, lines: Vec<roll_previews::ports::DrainLine>) {
        let lines = lines.into_iter().map(|l| apps_control::drains::PreviewLine { t: l.t, stderr: l.stderr, text: l.text }).collect();
        self.apps.drain_preview_lines(project, environment, service, source.as_str(), lines);
    }
}

/// Previews' OTLP intake reads spans with roll apps' reader.
pub struct AppsSpans;

impl roll_previews::ports::SpanReader for AppsSpans {
    fn spans(&self, body: &[u8], json: bool, gzip: bool) -> anyhow::Result<Vec<roll_previews::engine::traces::Span>> {
        Ok(apps_control::otlp::spans_of_request(body, json, gzip)?
            .into_iter()
            .map(|s| roll_previews::engine::traces::Span { trace: s.trace, span: s.span, parent: s.parent, name: s.name, service: s.service, start: s.start, end: s.end, error: s.error, attrs: s.attrs })
            .collect())
    }
}

pub struct AppsReach(pub Arc<apps_control::Apps>);

#[async_trait]
impl Reach for AppsReach {
    async fn reach(&self, service: &str, env: &str) -> anyhow::Result<Option<Reached>> {
        Ok(self.0.reachable(service, env).await?.map(|r| Reached { container: r.container.clone(), port: r.port, hold: Box::new(r) }))
    }
}

pub struct Reached {
    pub container: String,
    pub port: u16,
    /// Keeps the instance awake until dropped.
    pub hold: Box<dyn std::any::Any + Send + Sync>,
}

impl Services {
    /// Whether the runtime runs this service (it has a `roll.toml` spec and was started here).
    fn local(&self, preview: &str, service: &str) -> bool {
        use roll_previews::ports::Apps;
        self.runtime.status(preview, service).is_some()
    }
}

#[async_trait]
impl roll_previews::ports::Apps for Services {
    async fn catalog(&self, project: &str) -> anyhow::Result<Option<Vec<roll_previews::ports::CatalogService>>> {
        self.apps.catalog(project).await
    }

    async fn memory_mb(&self, preview: &str, service: &str) -> Option<u32> {
        self.apps.memory_mb(preview, service).await
    }

    async fn preview_domain(&self, project: &str) -> Option<(String, bool)> {
        self.apps.preview_domain(project).await
    }

    async fn deploy(&self, t: &roll_previews::ports::Target) -> anyhow::Result<Option<String>> {
        // Catalog services come without a roll.toml spec.
        if t.spec.image.is_empty() { self.apps.deploy(t).await } else { self.runtime.deploy(t).await }
    }

    async fn wake(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        match self.apps.find(preview, service).await? {
            Some(_) => self.apps.wake(preview, service).await,
            None => self.runtime.wake(preview, service).await,
        }
    }

    async fn sleep(&self, preview: &str, service: &str) {
        if self.local(preview, service) {
            self.runtime.sleep(preview, service).await
        } else {
            self.apps.sleep(preview, service).await
        }
    }

    async fn remove(&self, preview: &str, service: &str) -> anyhow::Result<()> {
        self.runtime.remove(preview, service).await?;
        self.apps.remove(preview, service).await
    }

    async fn logs(&self, preview: &str, service: &str, lines: usize) -> Vec<String> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.logs(preview, service, lines).await } else { self.runtime.logs(preview, service, lines).await }
    }

    /// The requests roll's runtime proxied (roll apps' instances are served by roll apps' own).
    fn requests(&self, preview: &str) -> Vec<roll_previews::RequestLine> {
        self.runtime.requests(preview)
    }

    async fn metrics(&self, preview: &str, service: &str) -> Option<(f64, u64)> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.metrics(preview, service).await } else { self.runtime.metrics(preview, service).await }
    }

    async fn live_build(&self, preview: &str, service: &str) -> Option<roll_previews::ports::LiveBuild> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.live_build(preview, service).await } else { None }
    }

    async fn browser_checks(&self, preview: &str, service: &str) -> Option<Vec<roll_previews::ports::BrowserCheckResult>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.browser_checks(preview, service).await } else { None }
    }

    async fn view_build(&self, preview: &str, service: &str, deployment: &str, plain: &[(String, String)], branches: &[roll_previews::ports::BranchVars]) -> anyhow::Result<Option<String>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { self.apps.view_build(preview, service, deployment, plain, branches).await } else { Ok(None) }
    }

    /// roll apps groups its own; a `roll.toml` service's are grouped here, from its last
    /// [`ERROR_LINES`] lines in the span. Grouping cuts a title and a sample, so when there's an
    /// error (and they're to be shown) its lines are masked first, then grouped again.
    async fn errors(&self, preview: &str, service: &str, minutes: i64, masks: Option<&roll_previews::engine::ErrorMasks<'_>>) -> roll_previews::Result<Vec<roll_previews::ErrorGroup>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) {
            return self.apps.errors(preview, service, minutes, masks).await;
        }
        let lines = self.runtime.timed_lines(preview, service, minutes, ERROR_LINES).await;
        let groups = group_errors(&lines);
        let Some(masks) = masks.filter(|_| !groups.is_empty()) else { return Ok(groups) };
        let values = masks.values().await?;
        let lines: Vec<_> = lines.into_iter().map(|(t, stderr, text)| (t, stderr, roll_previews::mask_cut(&text, values))).collect();
        Ok(group_errors(&lines))
    }

    async fn traces(&self, preview: &str, service: &str, minutes: i64) -> Vec<roll_previews::engine::traces::TraceRow> {
        self.apps.traces(preview, service, minutes).await
    }

    async fn trace(&self, preview: &str, service: &str, trace: &str) -> Option<Vec<roll_previews::engine::traces::Span>> {
        self.apps.trace(preview, service, trace).await
    }

    async fn follow(&self, preview: &str, service: &str, lines: usize) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { None } else { self.runtime.follow(preview, service, lines).await }
    }

    /// A `roll.toml` service's terminal: a login shell exec'd in its container through the Docker
    /// Engine API, as roll apps' shells are (roll apps' services have their own, on `pv-…`).
    async fn shell(&self, preview: &str, service: &str, cols: u16, rows: u16) -> anyhow::Result<Option<roll_previews::ports::Shell>> {
        if self.apps.find(preview, service).await?.is_some() {
            return Ok(None);
        }
        let Some(engine) = apps_control::runtime::engine::Engine::local() else { anyhow::bail!("a shell needs Docker on a local socket") };
        let container = self.runtime.shell_container(preview, service).await?;
        // bash when the image has it, else sh; a login shell, so the image's profile applies.
        let login = "if command -v bash >/dev/null 2>&1; then exec bash -l; else exec sh -l; fi";
        let (id, io) = engine.exec_tty(&container, &["sh", "-c", login], &["TERM=xterm-256color"], cols, rows).await?;
        Ok(Some(roll_previews::ports::Shell::upgraded(id, io)))
    }

    async fn resize_shell(&self, id: &str, cols: u16, rows: u16) -> anyhow::Result<()> {
        match apps_control::runtime::engine::Engine::local() {
            Some(e) => e.resize(id, cols, rows).await,
            None => Ok(()),
        }
    }

    fn hold(&self, preview: &str, service: &str) {
        self.runtime.hold(preview, service);
    }

    /// A `roll.toml` service's port (roll apps' services have roll apps' own tunnel, on `pv-…`).
    async fn forward(&self, preview: &str, service: &str, port: u16) -> anyhow::Result<Option<Box<dyn roll_previews::ports::ShellIo>>> {
        if self.apps.find(preview, service).await?.is_some() {
            return Ok(None);
        }
        self.runtime.forward(preview, service, port).await
    }

    /// A `roll.toml` service's page in a browser, its steps taken (roll apps' services: next).
    async fn try_page(&self, preview: &str, service: &str, path: &str, steps: &[roll_previews::settings::Step]) -> anyhow::Result<Option<roll_previews::ports::Tried>> {
        if self.apps.find(preview, service).await?.is_some() {
            return Ok(None);
        }
        self.runtime.try_page(preview, service, path, steps).await
    }

    /// A `roll.toml` service's page, read through the runtime (roll apps' services answer at their
    /// own URL, on `pv-…`).
    async fn fetch(&self, preview: &str, service: &str, url: &str, access: Option<&str>, path: &str, head: bool) -> anyhow::Result<Option<roll_previews::ports::Fetched>> {
        if self.apps.find(preview, service).await?.is_none() {
            return self.runtime.fetch(preview, service, url, access, path, head).await;
        }
        // roll apps serves it: through this node's own apps listener (its proxy, forwarding to
        // whichever node the instance runs on), by its host, as a visitor who came in by the
        // preview's link.
        let host = url.split_once("://").map_or(url, |(_, rest)| rest).split(['/', '?', '#']).next().unwrap_or("").to_string();
        anyhow::ensure!(!host.is_empty(), "{service} has no address yet");
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).timeout(std::time::Duration::from_secs(30)).build()?;
        let method = if head { reqwest::Method::HEAD } else { reqwest::Method::GET };
        let mut req = client
            .request(method, format!("http://{}{path}", loopback(self.apps_listen)))
            .header(reqwest::header::HOST, host)
            .header(reqwest::header::ACCEPT, "text/html,application/json,text/plain;q=0.9,*/*;q=0.5")
            .header(reqwest::header::ACCEPT_ENCODING, "identity");
        if let Some(t) = access {
            req = req.header(reqwest::header::COOKIE, format!("roll_access_{}={t}", roll_previews::branch_name(preview)));
        }
        let mut resp = req.send().await?;
        let status = resp.status().as_u16();
        const SHOWN: [&str; 4] = ["content-type", "location", "cache-control", "content-length"];
        let headers = SHOWN.iter().filter_map(|h| resp.headers().get(*h).and_then(|v| v.to_str().ok()).map(|v| (h.to_string(), v.chars().take(500).collect()))).collect();
        let (mut body, mut truncated) = (Vec::new(), false);
        while let Some(chunk) = resp.chunk().await? {
            let room = roll_previews::ports::FETCH_MAX - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        Ok(Some(roll_previews::ports::Fetched { status, headers, body, truncated }))
    }

    /// A `roll.toml` service can stand in for a laptop's process (roll apps' services are served by
    /// roll apps' proxy).
    async fn can_swap(&self, preview: &str, service: &str) -> anyhow::Result<bool> {
        Ok(self.apps.find(preview, service).await?.is_none())
    }

    fn swap(&self, preview: &str, service: &str, to: Option<roll_previews::ports::Laptop>) {
        self.runtime.swap(preview, service, to);
    }

    /// A `roll.toml` service's files (roll apps' services have their own, on `pv-…`).
    async fn read_files(&self, preview: &str, service: &str, path: &str) -> anyhow::Result<Option<roll_previews::ports::FileStream>> {
        if self.apps.find(preview, service).await?.is_some() {
            return Ok(None);
        }
        self.runtime.read_files(preview, service, path).await
    }

    async fn write_files(&self, preview: &str, service: &str, dir: &str, tar: bytes::Bytes) -> anyhow::Result<bool> {
        if self.apps.find(preview, service).await?.is_some() {
            return Ok(false);
        }
        self.runtime.write_files(preview, service, dir, tar).await
    }

    /// roll apps drains its own instances' output (a drain's `previews`).
    async fn drain_from(&self, preview: &str, service: &str, since: &str) -> Option<tokio::sync::mpsc::Receiver<(bool, String)>> {
        if matches!(self.apps.find(preview, service).await, Ok(Some(_))) { None } else { self.runtime.drain_from(preview, service, since).await }
    }

    async fn seed(&self, job: &roll_previews::ports::SeedJob) -> anyhow::Result<(i32, String)> {
        self.runtime.seed(job).await
    }

    async fn probe(&self, preview: &str, service: &str, path: &str) -> anyhow::Result<Option<(u16, String)>> {
        // roll apps checks its own deploys' health.
        match self.apps.find(preview, service).await? {
            Some(_) => Ok(None),
            None => self.runtime.probe(preview, service, path).await,
        }
    }

    async fn screenshot(&self, preview: &str, service: &str, opts: &roll_previews::ports::ShotOptions) -> anyhow::Result<Option<roll_previews::ports::Shot>> {
        let Some((id, env)) = self.apps.find(preview, service).await? else { return self.runtime.screenshot(preview, service, opts).await };
        // A roll apps instance: taken where it runs (its service's holder on a fleet).
        let Some(shots) = &self.shots else { return Ok(None) };
        super::fleet::shoot(self.fleet.as_ref(), shots.as_ref(), &id, &env, opts).await
    }

    fn screenshots_apps(&self) -> bool {
        self.shots.is_some()
    }

    fn status(&self, preview: &str, service: &str) -> Option<roll_previews::ServiceState> {
        self.runtime.status(preview, service)
    }

    // Frozen is the runtime's: without it a frozen service's dynamic credentials were revoked
    // while its process kept them (the engine's tend reads it).
    fn warm(&self, preview: &str, service: &str) -> bool {
        self.runtime.warm(preview, service)
    }

    /// roll apps' (the runtime's own counts go on the record as they happen).
    async fn intercepted(&self, preview: &str) -> Option<roll_previews::AppsIntercepted> {
        self.apps.intercepted(preview).await
    }

    /// What roll's runtime's proxy refused and what roll apps' did (its `pv-` environments):
    /// a preview's services can be on either.
    async fn egress_refused(&self, preview: &str) -> Vec<String> {
        let mut out = self.runtime.egress_refused(preview).await;
        for h in self.apps.egress_refused(preview).await {
            if !out.contains(&h) {
                out.push(h);
            }
        }
        out
    }
}

/// What a deploy of `key` finds of the one previews last started (`last`: its id and key).
#[derive(Debug, PartialEq)]
enum Already {
    /// It's the environment's latest, and still going: wait on it.
    UnderWay,
    /// It's what the environment runs: nothing to do.
    Live,
    /// Something else (another key, a newer deployment, a failed one): start one.
    No,
}

/// A redeploy's idempotency key: what it deploys and the deployment it replaces. A crash's retry
/// gets the same redeploy; variables back to an earlier set (v1 → v2 → v1) a new one, never the
/// earlier, superseded one.
fn redeploy_key(key: &str, replaces: &str) -> String {
    format!("redeploy:{key}:{replaces}")
}

fn already(last: Option<&(String, String)>, key: &str, env: &Value) -> Already {
    let Some((id, _)) = last.filter(|(_, k)| k == key) else { return Already::No };
    let going = matches!(env["latest"]["status"].as_str(), Some("queued" | "building" | "deploying"));
    if going && env["latest"]["id"].as_str() == Some(id.as_str()) {
        Already::UnderWay
    } else if env["active"]["id"].as_str() == Some(id.as_str()) {
        Already::Live
    } else {
        Already::No
    }
}

/// The most lines of a runtime service's output its errors are read from.
const ERROR_LINES: usize = 2_000;

/// A runtime service's lines (`(ms, from stderr, text)`, oldest first) grouped as roll apps groups
/// an environment's (traces joined per stream, then by fingerprint), the newest 50 (a new error
/// seen once isn't dropped for older, more frequent ones). Each read as roll apps' route answers
/// it, as [`RollApps::errors`] reads that: one shape for both.
fn group_errors(lines: &[(i64, bool, String)]) -> Vec<roll_previews::ErrorGroup> {
    use apps_control::logs::{LogLine, Stream, level_of};
    let lines: Vec<LogLine> = lines
        .iter()
        .map(|(t, stderr, text)| LogLine { t: *t, stream: if *stderr { Stream::Stderr } else { Stream::Stdout }, instance: None, deployment: None, level: level_of(text), text: text.clone(), trace: None })
        .collect();
    let mut groups = apps_control::errors::groups(&lines);
    groups.sort_by(|a, b| b.last.cmp(&a.last).then(b.count.cmp(&a.count)));
    groups.truncate(apps_control::errors::MAX_GROUPS);
    groups.iter().filter_map(|g| serde_json::to_value(g).ok().and_then(|v| serde_json::from_value(v).ok())).collect()
}

/// `addr`, but the loopback address when it listens on every interface.
fn loopback(mut addr: std::net::SocketAddr) -> std::net::SocketAddr {
    if addr.ip().is_unspecified() {
        addr.set_ip(std::net::Ipv4Addr::LOCALHOST.into());
    }
    addr
}

#[cfg(test)]
mod tests {
    /// Every variable of a roll apps link to a branched resource comes from the branch, under
    /// roll apps' names, or is blank: a stream's brokers and login, a bucket's endpoint and a
    /// search cluster's key or OpenSearch login never stay production's.
    #[test]
    fn a_links_variables_come_from_the_branch_or_are_blank() {
        let v = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect::<Vec<_>>();
        let stream = v(&[("KAFKA_BOOTSTRAP_SERVERS", "b:9092"), ("KAFKA_SASL_USERNAME", "pv"), ("KAFKA_SASL_PASSWORD", "s")]);
        assert_eq!(["KAFKA_BROKERS", "KAFKA_USERNAME", "KAFKA_PASSWORD"].map(|n| super::link_value(n, &stream)), [Some("b:9092".into()), Some("pv".into()), Some("s".into())]);
        assert_eq!(super::link_value("AWS_ENDPOINT_URL", &v(&[("AWS_ENDPOINT_URL_S3", "http://s3")])).as_deref(), Some("http://s3"));
        let es = v(&[("ELASTICSEARCH_URL", "https://br-1:p%40ss@search.test")]);
        assert_eq!(super::link_value("OPENSEARCH_USERNAME", &es).as_deref(), Some("br-1"));
        assert_eq!(super::link_value("OPENSEARCH_PASSWORD", &es).as_deref(), Some("p@ss"));
        assert_eq!(super::link_value("ELASTICSEARCH_API_KEY", &es).as_deref(), Some("YnItMTpwQHNz"), "base64(br-1:p@ss)");
        assert_eq!(super::link_value("ELASTICSEARCH_API_KEY", &v(&[("ELASTICSEARCH_URL", "https://u%40br-1:p@search.test")])), None, "a user@cluster login has no key form");
        // The branch's data-only credential, as production's link has: the URL without the
        // branch's own login, the key (or the data user, for OpenSearch) instead.
        let keyed = v(&[("ELASTICSEARCH_URL", "https://br-1:p@search.test"), ("ELASTICSEARCH_API_KEY", "ZGF0YQ==")]);
        assert_eq!(super::link_value("ELASTICSEARCH_URL", &keyed).as_deref(), Some("https://search.test"));
        assert_eq!(super::link_value("ELASTICSEARCH_API_KEY", &keyed).as_deref(), Some("ZGF0YQ=="));
        let os = v(&[("ELASTICSEARCH_URL", "https://br-1:p@search.test"), ("OPENSEARCH_USERNAME", "link@br-1"), ("OPENSEARCH_PASSWORD", "d")]);
        assert_eq!(super::link_value("OPENSEARCH_URL", &os).as_deref(), Some("https://link%40br-1:d@search.test"));
        assert_eq!(super::link_value("CASSANDRA_KEYSPACE", &v(&[("CASSANDRA_USERNAME", "pv")])), None, "nothing for it: blank, not production's");
    }

    use super::*;

    /// A `roll.toml` service's output is grouped from its stamped lines: a trace on stderr is one
    /// error however stdout's lines fall between, and the same error again is counted, not listed.
    /// Each group is read through roll apps' `…/errors` answer's JSON, as `RollApps` reads it.
    #[test]
    fn a_runtime_services_errors_are_grouped_from_its_stamped_lines() {
        let trace = |t: i64, line: u32| {
            vec![
                (t, true, "Traceback (most recent call last):".to_string()),
                (t, false, "GET / 200".to_string()),
                (t, true, format!("  File \"/app/main.py\", line {line}, in handler")),
                (t, true, "ZeroDivisionError: division by zero".to_string()),
            ]
        };
        let mut lines = trace(1_000, 10);
        lines.push((1_500, false, "ERROR payment 42 declined".into()));
        lines.extend(trace(2_000, 11));
        let g = group_errors(&lines);
        assert_eq!(g.len(), 2, "{g:#?}");
        assert_eq!((g[0].kind.as_str(), g[0].count, g[0].first_seen, g[0].last_seen), ("python", 2, 1_000, 2_000));
        assert_eq!(g[0].title, "ZeroDivisionError: division by zero");
        assert_eq!(g[0].sample.lines().count(), 3, "stdout's line isn't in it: {}", g[0].sample);
        assert_eq!((g[1].kind.as_str(), g[1].count), ("line", 1));
    }

    /// A poll while previews' own deploy is going waits on it, once it's live it's done, and only
    /// a different set of variables (or a deployment that replaced it) starts another: one
    /// deployment per change, not one per poll (26 a run before).
    #[test]
    fn a_poll_waits_on_its_own_deploy_and_starts_none_once_it_is_live() {
        let last = ("dep_2".to_string(), "pv-1@abc@00ff".to_string());
        let env = |latest: (&str, &str), active: &str| json!({ "latest": { "id": latest.0, "status": latest.1 }, "active": { "id": active } });
        assert_eq!(already(Some(&last), "pv-1@abc@00ff", &env(("dep_2", "building"), "dep_1")), Already::UnderWay);
        assert_eq!(already(Some(&last), "pv-1@abc@00ff", &env(("dep_2", "active"), "dep_2")), Already::Live);
        assert_eq!(already(Some(&last), "pv-1@abc@11ee", &env(("dep_2", "active"), "dep_2")), Already::No, "new variables");
        assert_eq!(already(Some(&last), "pv-1@abc@00ff", &env(("dep_3", "building"), "dep_2")), Already::Live, "someone else's on top: ours still runs");
        assert_eq!(already(Some(&last), "pv-1@abc@00ff", &env(("dep_2", "failed"), "dep_1")), Already::No, "it failed: try again");
        assert_eq!(already(None, "pv-1@abc@00ff", &env(("dep_1", "active"), "dep_1")), Already::No, "after a restart: start, keyed");
        // v1 (dep_1 live) → v2 → back to v1: that last redeploy isn't the first one again.
        let (v1, v2) = ("pv-1@abc@00ff", "pv-1@abc@11ee");
        assert_ne!(redeploy_key(v1, "dep_1"), redeploy_key(v1, "dep_2"));
        assert_ne!(redeploy_key(v1, "dep_2"), redeploy_key(v2, "dep_2"));
        assert!(redeploy_key(v1, "dep_123456789abc").len() <= 200, "roll apps' limit");
    }

    /// What roll set and no longer wants goes; what a person set stays; the list follows; what
    /// roll sets is sealed.
    #[test]
    fn a_removed_variable_is_unset_and_a_persons_own_kept() {
        let before = json!({ "ROLL_PREVIEW_SET": "FEATURE,DATABASE_URL", "FEATURE": "on", "DATABASE_URL": "x", "MINE": "set by hand" });
        let set: serde_json::Map<String, Value> = [("DATABASE_URL".to_string(), json!("y"))].into_iter().collect();
        let patch = variables_patch(&before, set);
        assert_eq!(patch["unset"], json!(["FEATURE"]));
        assert_eq!(patch["set"], json!({ "DATABASE_URL": "y", "ROLL_PREVIEW_SET": "DATABASE_URL" }));
        // Sealed (roll apps' logs hide it, its API never returns it), but not the list of names.
        assert_eq!(patch["seal"], json!(["DATABASE_URL"]));
        // A first deploy (nothing recorded): nothing unset.
        let first = variables_patch(&Value::Null, [("A".to_string(), json!("1"))].into_iter().collect());
        assert_eq!((first["unset"].clone(), first["set"]["ROLL_PREVIEW_SET"].clone()), (json!([]), json!("A")));
    }
}
