//! The machine's settings (docs/code/agents.md, "Machine settings"): what the owner set from roll
//! (`machine.configure`), kept in the store and laid over the daemon's `config.toml`. T3 Code
//! keeps these in its server settings (`apps/server/src/serverSettings.ts`, the provider instances
//! of `packages/contracts/src/settings.ts`); here the daemon keeps them, so they change the machine
//! that has them and nothing else.
//!
//! `config.toml` stays the machine's own file: it is never rewritten, and its credentials never
//! leave the machine (the report names its variables, never their values). Settings can't carry a
//! credential ([`MachineSettings::check`]).

use super::{Fail, Host, Res};
use crate::config::{Delegation, expand_tilde};
use crate::provider::{Provider, ProviderConfig};
use agent_proto::*;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// The store key of the machine's settings.
const KEY: &str = "machine";
/// New threads' branches when the settings name no prefix (`roll/<slug>`).
pub(crate) const BRANCH_PREFIX: &str = "roll";

impl Host {
    /// The stored settings; ones that no longer parse are dropped with a warning (a newer roll
    /// wrote them and this one was started again).
    pub(crate) fn stored_settings(store: &crate::store::Store) -> MachineSettings {
        match store.setting::<MachineSettings>(KEY) {
            Ok(s) => s.unwrap_or_default(),
            Err(e) => {
                tracing::warn!("ignoring the machine's stored settings: {e:#}");
                MachineSettings::default()
            }
        }
    }

    pub(crate) fn machine_settings(&self) -> MachineSettings {
        self.settings.lock().unwrap().clone()
    }

    /// A provider's config: `config.toml`'s entry with the machine's settings over it, and the
    /// endpoint it uses over both (`endpoints.rs`).
    pub(crate) fn provider_config(&self, id: &ProviderId) -> ProviderConfig {
        let cfg = merged(self.config.provider(id), &self.settings.lock().unwrap().provider(id));
        self.with_endpoint(id, cfg)
    }

    /// A provider new work may use: on this machine and not turned off in its settings.
    pub(crate) fn offered(&self, id: &ProviderId) -> Res<Arc<dyn Provider>> {
        let provider = self.provider(id)?;
        if !self.settings.lock().unwrap().provider(id).enabled {
            return Err(Fail::Provider(format!("{} is turned off on this machine: turn it on in the machine's settings", provider.name())));
        }
        Ok(provider)
    }

    pub(crate) fn worktree_dir(&self) -> PathBuf {
        self.settings.lock().unwrap().worktree_dir.as_deref().map(expand_tilde).unwrap_or_else(|| self.config.worktrees())
    }

    pub(crate) fn worktree_path(&self, project: &str, thread: &str) -> PathBuf {
        self.worktree_dir().join(project).join(thread)
    }

    /// Where roll code repos are cloned: the settings', else `config.toml`'s, else `~/roll`.
    pub(crate) fn clone_dir(&self) -> Option<PathBuf> {
        self.settings.lock().unwrap().clone_dir.as_deref().map(expand_tilde).or_else(|| self.config.clone_dir())
    }

    pub(crate) fn delegation(&self) -> Delegation {
        let s = self.settings.lock().unwrap();
        let base = self.config.delegation;
        Delegation { max_depth: s.max_depth.unwrap_or(base.max_depth), max_children: s.max_children.unwrap_or(base.max_children) }
    }

    /// How often auto-pull fetches plain git projects (`None`: never).
    pub(crate) fn pull_every(&self) -> Option<Duration> {
        match self.settings.lock().unwrap().pull_every {
            Some(0) => None,
            Some(s) => Some(Duration::from_secs(s)),
            None => self.config.pull_every(),
        }
    }

    pub(crate) fn branch_prefix(&self) -> String {
        self.settings.lock().unwrap().branch_prefix.clone().unwrap_or_else(|| BRANCH_PREFIX.into())
    }

    /// Who publishing commits as, when the settings say.
    pub(crate) fn git_identity(&self) -> Option<(String, String)> {
        let s = self.settings.lock().unwrap();
        s.git_name.clone().zip(s.git_email.clone())
    }

    /// `machine.configure`: check, store, apply (providers get their new config for their next
    /// process; auto-pull its new pace), then send `hello` again with the new info. The reply
    /// comes after that `hello`, so a client that has it sees the settings on the relay.
    pub(crate) async fn machine_configure(&self, settings: MachineSettings) -> Res<Reply> {
        let settings = settings.check().map_err(Fail::Invalid)?;
        if let Some(id) = settings.providers.keys().find(|id| !self.providers.contains_key(*id)) {
            return Err(Fail::Invalid(format!("no provider {} on this machine", id.0)));
        }
        let _guard = self.configuring.lock().await;
        self.store.put_setting(KEY, &settings)?;
        *self.settings.lock().unwrap() = settings;
        self.configure_providers();
        self.settings_changed.notify_waiters();
        // A provider's binary or variables may have changed: probe again, without waiting.
        self.probe_providers();
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    /// Gives every provider its config with the settings over it.
    pub(crate) fn configure_providers(&self) {
        for (id, provider) in &self.providers {
            provider.configure(self.provider_config(id));
        }
    }

    /// What the daemon says about the machine: the providers on offer as their probes left them
    /// (turned-off ones left out, and never probed; custom models added), and its settings with
    /// what they fall back to.
    pub(crate) async fn machine_info(&self) -> MachineInfo {
        let settings = self.machine_settings();
        let mut offered = Vec::with_capacity(self.providers.len());
        let mut providers = Vec::with_capacity(self.providers.len());
        for (id, provider) in &self.providers {
            let file = self.config.provider(id);
            let mut env: Vec<String> = file.env.iter().map(|(k, _)| k.clone()).collect();
            env.sort();
            env.dedup();
            providers.push(ProviderBase {
                id: id.clone(),
                name: provider.name(),
                binary: file.binary.map(|b| b.display().to_string()),
                args: file.args.len() as u32,
                env,
            });
            let set = settings.provider(id);
            if !set.enabled {
                continue;
            }
            let mut info = self.probes.info(provider.as_ref());
            for m in &set.models {
                if !info.models.iter().any(|known| known.id == m.id) {
                    let name = m.name.clone().unwrap_or_else(|| m.id.clone());
                    let options = provider.custom_model_options(&m.id, &info.models);
                    info.models.push(ModelInfo { custom: true, options, ..ModelInfo::new(m.id.clone(), name) });
                }
            }
            // An endpoint's models, under its name; and the provider's status says it uses it
            // (its own login no longer matters).
            if let Some(ep) = self.endpoint_of(id) {
                for m in &ep.models {
                    if !info.models.iter().any(|known| known.id == m.id) {
                        let options = provider.custom_model_options(&m.id, &info.models);
                        let name = m.name.clone().unwrap_or_else(|| m.id.clone());
                        info.models.push(ModelInfo { via: Some(ep.spec.name.clone()), options, ..ModelInfo::new(m.id.clone(), name) });
                    }
                }
                if info.installed {
                    info.auth = AuthStatus::Ready;
                    info.detail = Some(format!("via {}", ep.spec.name));
                }
            }
            offered.push(info);
        }
        let (git_name, git_email) = crate::git::global_identity().await;
        let base = SettingsBase {
            worktree_dir: self.config.worktrees().display().to_string(),
            branch_prefix: BRANCH_PREFIX.into(),
            clone_dir: self.config.clone_dir().map(|d| d.display().to_string()),
            pull_every: self.config.pull_every().map_or(0, |d| d.as_secs()),
            max_depth: self.config.delegation.max_depth,
            max_children: self.config.delegation.max_children,
            git_name,
            git_email,
        };
        MachineInfo {
            hostname: super::hostname(),
            os: std::env::consts::OS.into(),
            arch: std::env::consts::ARCH.into(),
            version: env!("CARGO_PKG_VERSION").into(),
            home: crate::config::user_home().unwrap_or_else(|| PathBuf::from("/")).to_string_lossy().into_owned(),
            providers: offered,
            settings: Some(Box::new(MachineConfig {
                set: settings,
                base,
                providers,
                config_path: self.config.home.join("config.toml").display().to_string(),
                endpoints: self.endpoint_infos(),
                gateway: None,
            })),
            seal_key: Some(self.seal_key()),
        }
    }
}

/// `config.toml`'s entry with a provider's settings over it: the settings' binary instead, their
/// arguments after the file's, and their variables over the file's of the same name.
fn merged(mut file: ProviderConfig, set: &ProviderSettings) -> ProviderConfig {
    if let Some(binary) = &set.binary {
        file.binary = Some(expand_tilde(binary));
    }
    file.args.extend(set.args.iter().cloned());
    file.env.retain(|(k, _)| !set.env.contains_key(k));
    file.env.extend(set.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    file.auto_compact_window = set.auto_compact_window;
    file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_lay_over_the_config_file() {
        let file = ProviderConfig {
            binary: Some("/usr/bin/claude".into()),
            args: vec!["--file".into()],
            env: vec![("ANTHROPIC_API_KEY".into(), "sk-file".into()), ("CLAUDE_CONFIG_DIR".into(), "/a".into())],
            ..Default::default()
        };
        assert_eq!(merged(file.clone(), &ProviderSettings::default()), file);
        let set = ProviderSettings {
            binary: Some("/opt/claude".into()),
            args: vec!["--set".into()],
            env: [("CLAUDE_CONFIG_DIR".to_string(), "/b".to_string()), ("RUST_LOG".to_string(), "info".to_string())].into(),
            ..ProviderSettings::default()
        };
        let got = merged(file, &set);
        assert_eq!(got.binary, Some(PathBuf::from("/opt/claude")));
        assert_eq!(got.args, vec!["--file", "--set"]);
        assert_eq!(
            got.env,
            vec![
                ("ANTHROPIC_API_KEY".to_string(), "sk-file".to_string()),
                ("CLAUDE_CONFIG_DIR".to_string(), "/b".to_string()),
                ("RUST_LOG".to_string(), "info".to_string()),
            ]
        );
    }
}
