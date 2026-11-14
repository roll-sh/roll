//! A service's environment when it starts, and what the runtime tells the engine back.

use super::*;

#[async_trait]
impl EnvSource for Engine {
    async fn env(&self, preview: &str, service: &str) -> anyhow::Result<(Target, Vec<(String, String)>)> {
        let r = self.store.row(preview)?;
        if r.desired == Desired::Deleted || matches!(r.state, State::AwaitingApproval | State::Queued | State::Deleting) {
            anyhow::bail!("the preview is {}", if r.desired == Desired::Deleted { "being deleted" } else { r.state.as_str() });
        }
        // A fork's head runs only once approved, checked again at every start.
        if let Some(change) = &r.change_id {
            match self.source.change(change).await? {
                Some(c) if c.may_run() && c.head == r.head => {}
                Some(c) if !c.may_run() => anyhow::bail!("this change's head isn't approved to run (approve-run)"),
                Some(_) => anyhow::bail!("the change moved on: the preview is updating"),
                None => anyhow::bail!("the change was deleted"),
            }
        }
        let s = self.store.settings(&r.project_id)?;
        let services = self.store.services(preview)?;
        // `web@<revision>`: an earlier push of `web`, at a host of its own.
        let (name, earlier) = match service.split_once('@') {
            Some((name, rev)) => (name, Some(rev)),
            None => (service, None),
        };
        let svc = services.iter().find(|x| x.name == name).ok_or_else(|| anyhow::anyhow!("no service {name}"))?;
        let (rev, host, url) = match earlier {
            Some(rev) => {
                // An approval is for one head: a fork's earlier heads don't run again.
                if !r.trusted {
                    anyhow::bail!("earlier pushes of a pull request from a fork don't run");
                }
                let p = self.store.pinned(preview)?.into_iter().find(|p| p.service == name && p.revision == rev).ok_or_else(|| anyhow::anyhow!("no earlier push {rev} of {name}"))?;
                let host = self.store.service_host(preview, service)?.ok_or_else(|| anyhow::anyhow!("no earlier push {rev} of {name}"))?;
                (p.revision, host, p.url)
            }
            None => {
                let rev = svc.revision.clone().ok_or_else(|| anyhow::anyhow!("{service} isn't deployed yet"))?;
                // The change's own repo runs only the head that was checked (and approved, for a fork).
                if svc.repo == r.repo && rev != r.build() {
                    anyhow::bail!("{service} is being redeployed at the change's new head");
                }
                (rev, svc.host.clone(), svc.url.clone())
            }
        };
        // Data being replaced, seeded or masked isn't served yet.
        let resources = self.store.resources(preview)?;
        if resources.iter().any(|x| matches!(x.state, crate::ResourceState::Pending | crate::ResourceState::Resetting)) || (s.seed.is_some() && self.store.seed_needed(preview)?) {
            anyhow::bail!("its data is being prepared (branched, reset, seeded or masked)");
        }
        let bytes: i64 = resources.iter().map(|x| x.bytes).sum();
        if bytes as u64 > s.max_bytes {
            anyhow::bail!("its branches hold {bytes} bytes, over the project's max_bytes: reset it to start again");
        }
        let spec = self
            .source
            .services(&svc.repo, &rev)
            .await?
            .into_iter()
            .find(|x| x.name == name)
            .ok_or_else(|| anyhow::anyhow!("{} no longer has a service {name} at {}", svc.repo, short(&rev)))?;
        // Later entries win: roll.toml, the secrets environment, the project's overrides, the
        // preview's own, the branches, then roll's own.
        let mut env = spec.env.clone();
        // A protected base (production) reaches a preview only when the project allows it and
        // the change is trusted; a fork's code never gets it.
        let untrusted = !(s.secrets.allow_production && r.trusted);
        for (k, v) in self.resolve_secrets(&r, untrusted).await? {
            set(&mut env, &k, v);
        }
        for (k, v) in project_env(&s, r.branch.as_deref()).into_iter().chain(self.store.vars(preview)?) {
            set(&mut env, &k, v);
        }
        for (k, v) in self.resource_env(preview).await? {
            set(&mut env, &k, v);
        }
        set(&mut env, "ROLL_PREVIEW", r.id.clone());
        set(&mut env, "ROLL_PREVIEW_URL", url);
        for other in &services {
            set(&mut env, &format!("ROLL_PREVIEW_URL_{}", env_suffix(&other.name)), other.url.clone());
        }
        set(&mut env, "PORT", spec.port.to_string());
        set(&mut env, "ROLL_REPO", svc.repo.clone());
        set(&mut env, "ROLL_CHANGESET", rev.clone());
        if let Some(c) = &r.change_id {
            set(&mut env, "ROLL_CHANGE", c.clone());
        }
        let t = Target {
            project: r.project_id.clone(),
            preview: r.id.clone(),
            service: service.to_string(),
            host,
            repo: svc.repo.clone(),
            revision: rev,
            git: None,
            spec,
            sleep_after: Duration::from_secs(s.sleep_after_s as u64),
            environment: env_name(&r.id),
            vars: Vec::new(),
            branches: Vec::new(),
            access_token: None,
            bypass_sha256: None,
            // The runtime's proxy adds the button per request (`runtime::Tagger`).
            feedback: false,
        };
        Ok((t, env))
    }

    fn touched(&self, preview: &str) {
        let _ = self.store.touch(preview);
    }
}

impl Engine {
    /// The runtime saw a service start, stop or fail: keep the preview's state truthful.
    pub async fn observed(&self, preview: &str, service: &str, state: ServiceState, error: Option<String>, active_ms: i64) {
        let _ = self.store.set_service(preview, service, state, None, error.as_deref());
        if active_ms > 0 {
            let _ = self.store.add_active(preview, service, active_ms);
        }
        let Ok(services) = self.store.services(preview) else { return };
        let running = services.iter().any(|s| s.state == ServiceState::Running);
        let Ok(row) = self.store.update(preview, |row| {
            if matches!(row.state, State::Ready | State::Sleeping) {
                row.state = if running { State::Ready } else { State::Sleeping };
                if running {
                    row.asleep = false;
                }
            }
        }) else {
            return;
        };
        let msg = match (state, &error) {
            (ServiceState::Running, _) => format!("{service} started"),
            (ServiceState::Sleeping, _) => format!("{service} went to sleep"),
            (ServiceState::Failed, Some(e)) => format!("{service} failed: {}", e.lines().next().unwrap_or("")),
            _ => return,
        };
        let _ = self.store.log(preview, &msg);
        self.report(row).await;
    }
}

