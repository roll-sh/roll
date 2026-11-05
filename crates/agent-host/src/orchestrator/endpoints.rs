//! `endpoint.set`, `endpoint.remove` and `endpoint.check` (docs/code/agents.md, "Endpoints"), and
//! how endpoints reach the providers: [`Host::provider_config`] lays the endpoint a provider uses
//! over its config ([`crate::provider::Provider::endpoint_use`]), so its next process gets the base
//! URL and key, and nothing else does (terminals, records, `hello`).

use super::{Fail, Host, Res, now};
use crate::endpoints::{self, Live, Stored};
use crate::provider::ProviderConfig;
use agent_proto::*;
use std::time::Instant;

impl Host {
    /// The stored endpoints; ones that no longer parse are dropped with a warning.
    pub(crate) fn stored_endpoints(store: &crate::store::Store) -> Vec<Live> {
        match store.setting::<Vec<Stored>>(endpoints::SETTING) {
            Ok(s) => s.unwrap_or_default().into_iter().map(Live::new).collect(),
            Err(e) => {
                tracing::warn!("ignoring the machine's stored endpoints: {e:#}");
                Vec::new()
            }
        }
    }

    fn save_endpoints(&self, list: &[Live]) -> Res<()> {
        let stored: Vec<&Stored> = list.iter().map(|l| &l.stored).collect();
        self.store.put_setting(endpoints::SETTING, &stored)?;
        Ok(())
    }

    /// `cfg` with the endpoint `id` uses laid over it, if it uses one.
    pub(crate) fn with_endpoint(&self, id: &ProviderId, cfg: ProviderConfig) -> ProviderConfig {
        let Ok(provider) = self.provider(id) else { return cfg };
        let live = self.endpoints.lock().unwrap().iter().find(|l| l.stored.spec.serves(id)).cloned();
        let Some(live) = live else { return cfg };
        let key = live.key();
        match provider.endpoint_use(&live.stored.spec, key.as_ref().map(|k| k.expose())) {
            Ok(using) => using.apply(cfg),
            // Checked when the endpoint was set; a provider that changed its mind since gets none.
            Err(e) => {
                tracing::warn!(provider = %id.0, endpoint = %live.stored.spec.id, "not using the endpoint: {e}");
                cfg
            }
        }
    }

    /// The endpoint a provider uses, for what `hello` says about it.
    pub(crate) fn endpoint_of(&self, id: &ProviderId) -> Option<EndpointInfo> {
        self.endpoints.lock().unwrap().iter().find(|l| l.stored.spec.serves(id)).map(Live::info)
    }

    pub(crate) fn endpoint_infos(&self) -> Vec<EndpointInfo> {
        self.endpoints.lock().unwrap().iter().map(Live::info).collect()
    }

    pub(crate) fn seal_key(&self) -> String {
        self.seal.public()
    }

    /// `endpoint.set`: check, open the key, store, give the providers their new config, send
    /// `hello` again, then reply. The models are listed in the background.
    pub(crate) async fn endpoint_set(&self, spec: EndpointSpec, key: KeyChange) -> Res<Reply> {
        let spec = spec.check().map_err(Fail::Invalid)?;
        for p in &spec.providers {
            let provider = self.provider(p)?;
            provider.endpoint_use(&spec, Some("k")).map_err(Fail::Invalid)?;
        }
        if spec.key_ref.is_some() && matches!(key, KeyChange::Set { .. }) {
            return Err(Fail::Invalid("an endpoint's key is sealed or a 1Password reference, not both".into()));
        }
        let opened = match &key {
            KeyChange::Set { sealed } => Some(self.seal.open(&spec.id, sealed).map_err(Fail::Invalid)?),
            _ => None,
        };
        let resolved = match &spec.key_ref {
            Some(r) => Some(endpoints::read_reference(r).await),
            None => None,
        };
        let _guard = self.configuring.lock().await;
        let mut list = self.endpoints.lock().unwrap().clone();
        let old = list.iter().position(|l| l.stored.spec.id == spec.id).map(|i| list.remove(i));
        if old.is_none() && list.len() >= endpoint_limits::ENDPOINTS {
            return Err(Fail::Invalid(format!("a machine has at most {} endpoints", endpoint_limits::ENDPOINTS)));
        }
        // A provider uses one endpoint: this one takes its providers from the others.
        for other in &mut list {
            other.stored.spec.providers.retain(|p| !spec.serves(p));
        }
        let same_server = old.as_ref().is_some_and(|o| o.stored.spec.base_url == spec.base_url && o.stored.spec.kind == spec.kind);
        let mut stored = Stored { spec: spec.clone(), key: None, models: Vec::new(), checked_at: None, error: None };
        if let Some(o) = &old
            && same_server
        {
            stored.models = o.stored.models.clone();
            stored.checked_at = o.stored.checked_at;
        }
        stored.key = match (key, spec.key_ref.is_some()) {
            (_, true) | (KeyChange::Clear, _) => None,
            (KeyChange::Set { .. }, false) => opened.map(|k| zeroize::Zeroizing::new(k.expose().to_string())),
            (KeyChange::Keep, false) => old.and_then(|o| o.stored.key),
        };
        let mut live = Live::new(stored);
        match resolved {
            Some(Ok(k)) => live.resolved = Some(k),
            Some(Err(e)) => live.ref_error = Some(e),
            None => {}
        }
        list.push(live);
        self.save_endpoints(&list)?;
        *self.endpoints.lock().unwrap() = list;
        self.configure_providers();
        self.probe_providers();
        self.publish_info().await;
        drop(_guard);
        self.spawn_listing(spec.id);
        Ok(Reply::Ok)
    }

    /// `endpoint.remove`: forget it and its key; its providers go back to their own config.
    pub(crate) async fn endpoint_remove(&self, id: &str) -> Res<Reply> {
        let _guard = self.configuring.lock().await;
        let mut list = self.endpoints.lock().unwrap().clone();
        let Some(i) = list.iter().position(|l| l.stored.spec.id == id) else {
            return Err(Fail::NotFound(format!("endpoint {id}")));
        };
        list.remove(i);
        self.save_endpoints(&list)?;
        *self.endpoints.lock().unwrap() = list;
        self.configure_providers();
        self.probe_providers();
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    /// `endpoint.check`: list the endpoint's models now and keep what it says.
    pub(crate) async fn endpoint_check(&self, id: &str) -> Res<Reply> {
        let found = self.endpoints.lock().unwrap().iter().find(|l| l.stored.spec.id == id).cloned();
        let Some(mut live) = found else { return Err(Fail::NotFound(format!("endpoint {id}"))) };
        if let Some(r) = live.stored.spec.key_ref.clone() {
            match endpoints::read_reference(&r).await {
                Ok(k) => (live.resolved, live.ref_error) = (Some(k), None),
                Err(e) => (live.resolved, live.ref_error) = (None, Some(e)),
            }
        }
        let started = Instant::now();
        let listed = match &live.ref_error {
            Some(e) => Err(e.clone()),
            None => endpoints::list_models(&live.stored.spec, live.key().as_ref().map(|k| k.expose())).await,
        };
        let ms = started.elapsed().as_millis().min(u32::MAX as u128) as u32;
        let check = match &listed {
            Ok(models) => EndpointCheck { ok: true, models: models.len() as u32, ms, error: None },
            Err(e) => EndpointCheck { ok: false, models: 0, ms, error: Some(e.clone()) },
        };
        self.keep_listing(&live, listed).await?;
        Ok(Reply::EndpointChecked(check))
    }

    /// Lists an endpoint's models in the background (after it is set, and at start for those
    /// never listed) and says `hello` again with them.
    pub(crate) fn spawn_listing(&self, id: String) {
        let me = self.me.clone();
        tokio::spawn(async move {
            let Some(host) = me.upgrade() else { return };
            if let Err(e) = host.endpoint_check(&id).await {
                tracing::debug!(endpoint = %id, "listing the endpoint's models: {e:?}");
            }
        });
    }

    /// At start: read 1Password references (memory only), then list what was never listed.
    pub(crate) fn spawn_endpoints_start(&self) {
        let me = self.me.clone();
        tokio::spawn(async move {
            let Some(host) = me.upgrade() else { return };
            let refs: Vec<(String, String)> =
                host.endpoints.lock().unwrap().iter().filter_map(|l| Some((l.stored.spec.id.clone(), l.stored.spec.key_ref.clone()?))).collect();
            if !refs.is_empty() {
                for (id, r) in refs {
                    let got = endpoints::read_reference(&r).await;
                    if let Some(l) = host.endpoints.lock().unwrap().iter_mut().find(|l| l.stored.spec.id == id) {
                        match got {
                            Ok(k) => (l.resolved, l.ref_error) = (Some(k), None),
                            Err(e) => (l.resolved, l.ref_error) = (None, Some(e)),
                        }
                    }
                }
                host.configure_providers();
                host.probe_providers();
                host.publish_info().await;
            }
            let unlisted: Vec<String> = host.endpoints.lock().unwrap().iter().filter(|l| l.stored.checked_at.is_none()).map(|l| l.stored.spec.id.clone()).collect();
            for id in unlisted {
                host.spawn_listing(id);
            }
        });
    }

    /// Keeps a listing's outcome on the endpoint (if it is still the same endpoint) and says
    /// `hello` again.
    async fn keep_listing(&self, was: &Live, listed: Result<Vec<EndpointModel>, String>) -> Res<()> {
        let _guard = self.configuring.lock().await;
        let mut list = self.endpoints.lock().unwrap().clone();
        let Some(l) = list.iter_mut().find(|l| l.stored.spec == was.stored.spec) else { return Ok(()) };
        l.stored.checked_at = Some(now());
        l.resolved = was.resolved.clone();
        l.ref_error = was.ref_error.clone();
        match listed {
            Ok(models) => (l.stored.models, l.stored.error) = (models, None),
            Err(e) => l.stored.error = Some(e),
        }
        self.save_endpoints(&list)?;
        *self.endpoints.lock().unwrap() = list;
        // A reference read again may change the key the providers get.
        self.configure_providers();
        self.publish_info().await;
        Ok(())
    }
}
