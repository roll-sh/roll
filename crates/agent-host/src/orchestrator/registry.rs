//! `registry.list`, `registry.install` and `registry.remove` (docs/code/agents.md, "The ACP
//! registry"; `crate::registry` does the fetching and installing). T3 Code adds a registry agent
//! as a provider instance from its settings (`AddProviderInstanceDialog.tsx`, `prepare` in
//! `AcpRegistrySupport.ts`); here an install adds the provider `acp:<id>` to the running daemon:
//! it is configured, probed in the background, and `hello` says so, with no restart.

use super::{Fail, Host, Res};
use crate::provider::{Provider, ProviderConfig};
use crate::registry::{self, LIST_MAX_AGE, index::Agent};
use agent_proto::*;
use std::sync::Arc;

impl Host {
    /// The config a provider starts from: `config.toml`'s, or, for an agent installed from the
    /// registry that `config.toml` doesn't name, what was installed.
    pub(crate) fn base_config(&self, id: &ProviderId) -> ProviderConfig {
        if let Some(name) = id.as_str().strip_prefix("acp:")
            && !self.config.acp.iter().any(|a| a.name == name)
            && let Some(i) = self.registry.installed_one(name)
        {
            return i.agent().config;
        }
        self.config.provider(id)
    }

    /// Offers a provider from now on: configured, then probed in the background (`hello` says
    /// `checking` at once, and its status when the probe answers).
    pub(crate) async fn add_provider(&self, provider: Arc<dyn Provider>) {
        let id = provider.id();
        self.providers.write().unwrap().insert(id.clone(), provider.clone());
        provider.configure(self.provider_config(&id));
        if self.machine_settings().provider(&id).enabled {
            self.probe(&id, provider);
        }
        self.publish_info().await;
    }

    /// The provider already on this machine that is `agent` (under its own id, a known agent's
    /// name or a native driver), if one is installed.
    fn offered_as(&self, agent: &Agent) -> Option<ProviderId> {
        let ours = self.registry.installed_one(&agent.id).map(|i| i.provider_id());
        registry::same_as(&agent.id).into_iter().find(|id| {
            Some(id) == ours.as_ref() || self.provider(id).is_ok_and(|p| self.probes.info(p.as_ref()).installed)
        })
    }

    pub(crate) async fn registry_list(&self) -> Res<Reply> {
        let listed = self.registry.index(Some(LIST_MAX_AGE)).await;
        let mut agents = Vec::new();
        for a in listed.index.iter().flat_map(|i| i.agents.iter()) {
            let choices = a.choices(registry::index::platform());
            let usable = self.registry.choose(a);
            let integrity = usable.or(choices.first().copied()).map_or(RegistryIntegrity::None, |c| c.integrity());
            agents.push(RegistryAgent {
                id: a.id.clone(),
                name: a.name.clone(),
                version: a.version.clone(),
                description: a.description.clone(),
                website: a.website.clone(),
                repository: a.repository.clone(),
                icon: a.icon.clone(),
                kinds: choices.iter().map(|c| c.kind()).collect(),
                installable: usable.is_some(),
                integrity,
                installed: self.registry.installed_one(&a.id).map(|i| i.version),
                offered: self.offered_as(a),
            });
        }
        Ok(Reply::Registry(RegistryList { agents, fetched_at: listed.fetched_at, error: listed.error }))
    }

    /// Installs `id` from a fresh copy of the registry (the kept one when it can't be reached),
    /// then offers it. Installing the version already installed again only offers it.
    pub(crate) async fn registry_install(&self, id: &str) -> Res<Reply> {
        if !is_registry_id(id) {
            return Err(Fail::Invalid(format!("{id:?} isn't an ACP registry id")));
        }
        let listed = self.registry.index(None).await;
        let index = listed.index.ok_or_else(|| Fail::Provider(listed.error.clone().unwrap_or_default()))?;
        let agent = index.agents.iter().find(|a| a.id == id).ok_or_else(|| Fail::NotFound(format!("the ACP registry has no agent {id}")))?;
        let pid = ProviderId(format!("acp:{id}"));
        let ours = self.registry.installed_one(id);
        if ours.is_none() {
            if let Some(other) = self.offered_as(agent) {
                return Err(Fail::Conflict(format!("{} is on this machine already, as the provider {}", agent.name, other.0)));
            }
            if self.has_provider(&pid) {
                return Err(Fail::Conflict(format!("config.toml's [[acp]] {id} has this agent's name: remove it there to install {} from the registry", agent.name)));
            }
        }
        let installed = match ours.filter(|o| o.version == agent.version && o.command.exists()) {
            Some(same) => same,
            None => self.registry.install(agent).await.map_err(|e| Fail::Provider(format!("installing {}: {e:#}", agent.name)))?,
        };
        match self.provider(&pid) {
            // A new version of one it offers: its next process is the new one.
            Ok(p) => {
                p.configure(self.provider_config(&pid));
                self.probe(&pid, p);
                self.publish_info().await;
            }
            Err(_) => self.add_provider(installed.provider()).await,
        }
        Ok(Reply::Ok)
    }

    /// Removes an agent installed from the registry: refused while one of its threads runs; its
    /// idle sessions close, then its provider, entry and files go.
    pub(crate) async fn registry_remove(&self, id: &str) -> Res<Reply> {
        let Some(installed) = self.registry.installed_one(id) else {
            return Err(Fail::NotFound(format!("{id} wasn't installed from the ACP registry on this machine")));
        };
        let pid = installed.provider_id();
        let configured = self.config.acp.iter().any(|a| a.name == id);
        if !configured {
            for entity in self.store.list(EntityKind::Thread)? {
                if let Entity::Thread(t) = entity
                    && t.provider == pid
                    && (t.status.is_busy() || t.active_run.is_some())
                {
                    return Err(Fail::Conflict(format!("{} is running {:?}: stop it first", installed.name, t.title)));
                }
            }
            self.providers.write().unwrap().remove(&pid);
            self.forget_probe(&pid);
            let threads: Vec<_> = self.threads.lock().unwrap().values().cloned().collect();
            for rt in threads {
                let mut rt = rt.lock().await;
                if !rt.gone && rt.live.as_ref().is_some_and(|l| l.provider == pid) && rt.active.is_none() {
                    self.close_session(&mut rt).await;
                }
            }
        }
        self.registry.remove(id).map_err(|e| Fail::Conflict(format!("{e:#}")))?;
        self.publish_info().await;
        Ok(Reply::Ok)
    }
}
