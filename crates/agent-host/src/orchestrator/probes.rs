//! Provider probes (docs/code/agents.md, "Providers"): the daemon says hello at once, and each
//! provider's status fills in as its own probe answers. T3 Code's provider registry does the
//! same: it starts from its status cache (`apps/server/src/provider/providerStatusCache.ts`) and
//! refreshes each provider on its own (`ProviderRegistry.ts`).
//!
//! - Each provider keeps its last probe in the store, so a restart starts from it, marked
//!   `checking` until the new probe answers. With no last probe it shows as not installed yet.
//! - A newer probe supersedes one still running, whose answer is dropped.
//! - Every probe that finishes sends `hello` again with the machine info.
//! - A command that reads a provider's info waits for its probe, at most [`PROBE_WAIT`].
//! - What a live session offered ([`Probes::learn`], ACP agents' models and options, which
//!   `initialize` doesn't say) is kept per provider and version, and fills in a probe that found
//!   no models (T3's `applyAcpRegistryLiveConfiguration`).

use super::Host;
use crate::Outbound;
use crate::provider::Provider;
use agent_proto::{AuthStatus, ModelInfo, ProviderId, ProviderInfo, UsageLimits};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The store key of the last probes.
const KEY: &str = "provider_probes";
/// The store key of what live sessions offered.
const LEARNED: &str = "provider_catalogs";
/// How long a command waits for a probe. Longer than any provider's own timeouts (a `--version`
/// and then a 20 s probe), so it only runs out when a probe is stuck.
const PROBE_WAIT: Duration = Duration::from_secs(45);

#[derive(Default)]
pub(crate) struct Probes {
    state: std::sync::Mutex<BTreeMap<ProviderId, Probe>>,
    next: AtomicU64,
    /// What live sessions offered, by provider.
    learned: std::sync::Mutex<BTreeMap<ProviderId, Learned>>,
}

/// A live session's models, for the provider version that offered them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Learned {
    #[serde(default)]
    version: Option<String>,
    models: Vec<ModelInfo>,
}

#[derive(Default)]
struct Probe {
    /// What the last probe that finished said.
    last: Option<ProviderInfo>,
    /// The probe running now.
    running: Option<u64>,
}

impl Probes {
    /// The last probes from the store; ones that no longer parse are dropped.
    pub(crate) fn load(store: &crate::store::Store) -> Probes {
        let last: BTreeMap<ProviderId, ProviderInfo> = store.setting(KEY).unwrap_or_else(|e| {
            tracing::warn!("ignoring the stored provider probes: {e:#}");
            None
        }).unwrap_or_default();
        let state = last.into_iter().map(|(id, info)| (id, Probe { last: Some(info), running: None })).collect();
        let learned: BTreeMap<ProviderId, Learned> = store.setting(LEARNED).unwrap_or_else(|e| {
            tracing::warn!("ignoring the stored provider catalogs: {e:#}");
            None
        }).unwrap_or_default();
        Probes { state: std::sync::Mutex::new(state), next: AtomicU64::new(0), learned: std::sync::Mutex::new(learned) }
    }

    /// A live session of `provider` offered `models`: kept (and stored) for the version its last
    /// probe found. True when that changed what the provider offers.
    pub(crate) fn learn(&self, provider: &ProviderId, models: Vec<ModelInfo>, store: &crate::store::Store) -> bool {
        let version = self.state.lock().unwrap().get(provider).and_then(|p| p.last.as_ref()).and_then(|i| i.version.clone());
        let next = Learned { version, models };
        let mut learned = self.learned.lock().unwrap();
        if learned.get(provider) == Some(&next) {
            return false;
        }
        learned.insert(provider.clone(), next);
        if let Err(e) = store.put_setting(LEARNED, &*learned) {
            tracing::warn!("couldn't keep the provider catalogs: {e:#}");
        }
        true
    }

    /// What's known about `provider` now.
    pub(crate) fn info(&self, provider: &dyn Provider) -> ProviderInfo {
        let state = self.state.lock().unwrap();
        let probe = state.get(&provider.id());
        let mut info = probe.and_then(|p| p.last.clone()).unwrap_or_else(|| ProviderInfo {
            id: provider.id(),
            name: provider.name(),
            installed: false,
            version: None,
            auth: AuthStatus::Unknown,
            detail: None,
            models: Vec::new(),
            update_required: Vec::new(),
            capabilities: provider.capabilities(),
            checking: false,
            usage_limits: None,
        });
        info.checking = probe.is_some_and(|p| p.running.is_some());
        drop(state);
        // A probe that can't list models without a session: what the last one offered, if it
        // was this version.
        if info.models.is_empty()
            && info.installed
            && let Some(learned) = self.learned.lock().unwrap().get(&provider.id())
            && learned.version == info.version
        {
            info.models = learned.models.clone();
        }
        info
    }
}

impl Host {
    /// Probes every provider on offer, each on its own; `hello` goes out again as each answers.
    /// Turned-off providers are never probed.
    pub(crate) fn probe_providers(&self) {
        let settings = self.machine_settings();
        for (id, provider) in &self.providers {
            if !settings.provider(id).enabled {
                continue;
            }
            let probe = self.probes.next.fetch_add(1, Ordering::Relaxed);
            self.probes.state.lock().unwrap().entry(id.clone()).or_default().running = Some(probe);
            let (me, provider) = (self.me.clone(), Arc::clone(provider));
            tokio::spawn(async move {
                let info = provider.info().await;
                let Some(host) = me.upgrade() else { return };
                let current = {
                    let mut state = host.probes.state.lock().unwrap();
                    let entry = state.entry(provider.id()).or_default();
                    let current = entry.running == Some(probe);
                    if current {
                        let mut info = info;
                        // Limits sessions reported since stay unless the probe saw newer ones.
                        let published = entry.last.as_mut().and_then(|l| l.usage_limits.take());
                        info.usage_limits = UsageLimits::after_probe(published, info.usage_limits.take());
                        *entry = Probe { last: Some(info), running: None };
                    }
                    current
                };
                if current {
                    host.publish_info().await;
                }
            });
        }
    }

    /// A session reported `provider`'s usage limits: merge them into its info and say `hello`
    /// again when they changed. A provider with no probe yet keeps them for when it has one.
    pub(crate) fn usage_limits(&self, provider: &ProviderId, update: &UsageLimits) {
        let changed = {
            let mut state = self.probes.state.lock().unwrap();
            let Some(info) = state.get_mut(provider).and_then(|p| p.last.as_mut()) else { return };
            match UsageLimits::apply(info.usage_limits.as_ref(), update) {
                Some(next) => {
                    info.usage_limits = Some(next);
                    true
                }
                None => false,
            }
        };
        if changed {
            let me = self.me.clone();
            tokio::spawn(async move {
                if let Some(host) = me.upgrade() {
                    host.publish_info().await;
                }
            });
        }
    }

    /// Whether an installed provider's last probe said it isn't logged in.
    pub(crate) fn logged_out(&self) -> bool {
        self.probes.state.lock().unwrap().values().any(|p| p.last.as_ref().is_some_and(|i| i.installed && i.auth == AuthStatus::Missing))
    }

    /// Gathers the machine info, keeps the probes for the next start, and sends `hello` with
    /// them. One at a time, so the newest info is the one sent last.
    pub(crate) async fn publish_info(&self) {
        let _guard = self.publishing.lock().await;
        let last: BTreeMap<ProviderId, ProviderInfo> =
            self.probes.state.lock().unwrap().iter().filter_map(|(id, p)| Some((id.clone(), p.last.clone()?))).collect();
        if let Err(e) = self.store.put_setting(KEY, &last) {
            tracing::warn!("couldn't keep the provider probes: {e:#}");
        }
        let info = self.machine_info().await;
        self.info.send_replace(info.clone());
        let _ = self.out.send(Outbound::Info(info));
    }

    /// Waits until `provider`'s probe (every provider's, with `None`) has answered, at most
    /// [`PROBE_WAIT`]; after that the caller goes on with what's known.
    pub(crate) async fn probed(&self, provider: Option<&ProviderId>) {
        let mut info = self.info.subscribe();
        let done = |i: &agent_proto::MachineInfo| i.providers.iter().all(|p| !p.checking || provider.is_some_and(|id| p.id != *id));
        if tokio::time::timeout(PROBE_WAIT, info.wait_for(done)).await.is_err() {
            tracing::warn!("a provider probe is taking longer than {} s; going on without it", PROBE_WAIT.as_secs());
        }
    }
}
