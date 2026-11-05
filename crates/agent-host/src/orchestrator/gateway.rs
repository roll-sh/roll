//! The `gateway.*` commands (docs/code/gateway.md): this daemon hosting roll's model gateway
//! (`crate::gateway`). Each command that changes something stores it, applies it, says `hello`
//! again and only then replies. A client key is minted by the gateway, sealed at once to the
//! machine that will use it (as its `roll-gateway` endpoint's key) and dropped: it is never
//! stored here, logged or put in a reply in the clear.
//!
//! Also the member side's hooks: whether a provider's gateway endpoint is used for its next
//! process (the fallback) and the per-thread attribution variables.

use super::{Fail, Host, Res};
use crate::endpoints::{self, Live};
use crate::gateway::{self, Hosted, Stored};
use agent_proto::*;
use std::collections::BTreeMap;
use std::sync::Weak;

/// The hosted gateway: commands change it one at a time, readers take a clone.
#[derive(Default)]
pub(crate) struct Hosting {
    changing: tokio::sync::Mutex<()>,
    live: std::sync::Mutex<Option<Hosted>>,
}

impl Host {
    fn hosted(&self) -> Option<Hosted> {
        self.gateway.live.lock().unwrap().clone()
    }

    fn hosted_or_404(&self) -> Res<Hosted> {
        self.hosted().ok_or_else(|| Fail::NotFound("there's no gateway on this machine: set it up first".into()))
    }

    /// What `hello` says about the hosted gateway.
    pub(crate) fn gateway_info(&self) -> Option<Box<GatewayInfo>> {
        self.hosted().map(|h| Box::new(h.info()))
    }

    fn gateway_dir(&self) -> std::path::PathBuf {
        self.config.home.join("gateway")
    }

    /// At start: a gateway set up here before runs again (in the background: 1Password may wait
    /// on its unlock).
    pub(crate) fn spawn_gateway_start(&self) {
        let stored = match self.store.setting::<Stored>(gateway::SETTING) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => return tracing::warn!("ignoring the machine's stored gateway: {e:#}"),
        };
        let me = self.me.clone();
        tokio::spawn(async move {
            let Some(host) = me.upgrade() else { return };
            let _guard = host.gateway.changing.lock().await;
            if let Err(e) = host.gateway_apply(stored).await {
                tracing::warn!("starting the gateway: {e:?}");
            }
            host.publish_info().await;
        });
    }

    /// Opens the gateway if it isn't, reads references, and applies `stored`. Callers hold
    /// `changing` and have stored it.
    async fn gateway_apply(&self, stored: Stored) -> Res<Hosted> {
        let previous = self.hosted();
        let gw = match &previous {
            Some(h) => h.gateway.clone(),
            None => {
                let dir = self.gateway_dir();
                std::fs::create_dir_all(&dir).map_err(|e| Fail::Internal(anyhow::anyhow!("making {}: {e}", dir.display())))?;
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
                }
                model_gateway::Gateway::open(&dir).await?
            }
        };
        let refs = gateway::resolve_refs(&stored.settings, previous.as_ref().map_or(&BTreeMap::new(), |h| &h.refs)).await;
        let watcher = match previous.as_ref().and_then(|h| h.watcher.clone()) {
            Some(w) => Some(w),
            None => Some(spawn_watcher(self.me.clone(), gw.clone())),
        };
        let hosted = Hosted { gateway: gw, stored, refs, watcher };
        let run = hosted.gateway.apply(hosted.stored.settings.clone(), hosted.keys()).await;
        tracing::info!("the gateway is {}", describe(&run));
        *self.gateway.live.lock().unwrap() = Some(hosted.clone());
        Ok(hosted)
    }

    /// `gateway.configure`: check, store (keys of upstreams that are gone, and sealed keys of
    /// upstreams that now read 1Password, are forgotten), apply, `hello`, reply.
    pub(crate) async fn gateway_configure(&self, settings: GatewaySettings) -> Res<Reply> {
        let settings = settings.check().map_err(Fail::Invalid)?;
        let _guard = self.gateway.changing.lock().await;
        let mut stored = self.hosted().map(|h| h.stored).unwrap_or_default();
        stored.keys.retain(|id, _| settings.upstream(id).is_some_and(|u| u.key_ref.is_none()));
        stored.settings = settings;
        self.store.put_setting(gateway::SETTING, &stored)?;
        self.gateway_apply(stored).await?;
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    /// `gateway.key`: an upstream's key, sealed to this daemon for that upstream, or cleared.
    pub(crate) async fn gateway_key(&self, upstream: &str, key: KeyChange) -> Res<Reply> {
        let _guard = self.gateway.changing.lock().await;
        let hosted = self.hosted_or_404()?;
        let spec = hosted.stored.settings.upstream(upstream).ok_or_else(|| Fail::NotFound(format!("upstream {upstream}")))?;
        let mut stored = hosted.stored.clone();
        match key {
            KeyChange::Keep => return Ok(Reply::Ok),
            KeyChange::Clear => {
                stored.keys.remove(upstream);
            }
            KeyChange::Set { sealed } => {
                if spec.key_ref.is_some() {
                    return Err(Fail::Invalid(format!("{upstream}'s key is read from 1Password: clear the reference to seal one")));
                }
                let opened = self.seal.open_with(&gateway::upstream_key_context(upstream), &sealed).map_err(Fail::Invalid)?;
                stored.keys.insert(upstream.to_string(), zeroize::Zeroizing::new(opened.expose().to_string()));
            }
        }
        self.store.put_setting(gateway::SETTING, &stored)?;
        self.gateway_apply(stored).await?;
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    /// `gateway.client.issue`: a new client key, sealed to `seal_key` as the `roll-gateway`
    /// endpoint's key (the member's own `endpoint.set` opens it), and dropped.
    pub(crate) async fn gateway_client_issue(&self, name: &str, machine: Option<MachineId>, seal_key: &str) -> Res<Reply> {
        let name = name.trim();
        if name.is_empty() || name.len() > gateway_limits::NAME || name.chars().any(char::is_control) {
            return Err(Fail::Invalid(format!("a client's name is one line of 1 to {} bytes", gateway_limits::NAME)));
        }
        let _guard = self.gateway.changing.lock().await;
        let hosted = self.hosted_or_404()?;
        let (client, key) = hosted.gateway.issue_client(name, machine)?;
        let sealed = match endpoints::seal(seal_key, gateway_limits::ENDPOINT_ID, key.expose()) {
            Ok(s) => s,
            Err(e) => {
                drop(key);
                // Nobody can ever use it: take it back.
                if let Err(e) = hosted.gateway.revoke_client(&client.id) {
                    tracing::warn!(client = %client.id, "revoking a client key nobody got: {e:#}");
                }
                return Err(Fail::Invalid(format!("the machine's seal key isn't one: {e}")));
            }
        };
        drop(key);
        self.publish_info().await;
        Ok(Reply::GatewayClient(IssuedClient { client, sealed }))
    }

    pub(crate) async fn gateway_client_revoke(&self, client: &str) -> Res<Reply> {
        let _guard = self.gateway.changing.lock().await;
        let hosted = self.hosted_or_404()?;
        if !hosted.gateway.revoke_client(client)? {
            return Err(Fail::NotFound(format!("client {client}")));
        }
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    pub(crate) async fn gateway_test(&self, upstream: Option<&str>) -> Res<Reply> {
        let hosted = self.hosted_or_404()?;
        if let Some(u) = upstream
            && hosted.stored.settings.upstream(u).is_none()
        {
            return Err(Fail::NotFound(format!("upstream {u}")));
        }
        Ok(Reply::GatewayTested { checks: hosted.gateway.test(upstream).await })
    }

    /// `gateway.remove`: stop it and forget it here: settings, keys, clients and history. Done
    /// when there's nothing to remove.
    pub(crate) async fn gateway_remove(&self) -> Res<Reply> {
        let _guard = self.gateway.changing.lock().await;
        let hosted = self.gateway.live.lock().unwrap().take();
        self.store.delete_setting(gateway::SETTING)?;
        if let Some(h) = hosted {
            if let Some(w) = &h.watcher {
                w.abort();
            }
            h.gateway.wipe().await?;
        }
        let dir = self.gateway_dir();
        if let Err(e) = std::fs::remove_dir_all(&dir)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("removing {}: {e}", dir.display());
        }
        self.publish_info().await;
        Ok(Reply::Ok)
    }

    pub(crate) async fn gateway_status(&self) -> Res<Reply> {
        Ok(Reply::GatewayStatus(self.hosted_or_404()?.gateway.status().await))
    }

    pub(crate) async fn gateway_usage(&self, from: Millis, to: Millis, by: UsageBy, bucket: UsageBucket) -> Res<Reply> {
        if from > to {
            return Err(Fail::Invalid("usage is from a time to a later one".into()));
        }
        let gw = self.hosted_or_404()?.gateway;
        Ok(Reply::GatewayUsage(gw.usage(from, to, by, bucket)?))
    }

    pub(crate) async fn gateway_log(&self, after: Option<u64>, before: Option<u64>, limit: u16, errors: bool) -> Res<Reply> {
        let limit = if limit == 0 { gateway_limits::LOG_PAGE } else { limit.min(gateway_limits::LOG_PAGE) };
        let gw = self.hosted_or_404()?.gateway;
        Ok(Reply::GatewayLog { entries: gw.log(after, before, limit, errors)? })
    }

    pub(crate) async fn gateway_log_body(&self, seq: u64) -> Res<Reply> {
        let gw = self.hosted_or_404()?.gateway;
        gw.log_body(seq)?.map(Reply::GatewayLogBody).ok_or_else(|| Fail::NotFound(format!("no bodies kept for request {seq}")))
    }

    /// The daemon is stopping: the gateway stops listening (requests in flight finish).
    pub(crate) async fn stop_gateway(&self) {
        let Some(h) = self.hosted() else { return };
        if let Some(w) = &h.watcher {
            w.abort();
        }
        h.gateway.stop().await;
    }

    // ── the member side ──

    /// Whether `live`'s variables are left off a provider's processes: a gateway endpoint with
    /// `fallback` that didn't answer its last health check.
    pub(crate) fn fallen_back(&self, live: &Live) -> bool {
        let spec = &live.stored.spec;
        spec.kind == EndpointKind::Gateway && spec.fallback && self.gateway_health.known_down(&spec.base_url)
    }

    /// Before a provider's process starts: if it uses a gateway endpoint with `fallback`, ask the
    /// gateway whether it answers (cached), and when that changed, give the providers their new
    /// config and say so in `hello` (the status says "roll gateway offline: using its own login").
    pub(crate) async fn check_gateway(&self, id: &ProviderId) {
        let base = {
            let endpoints = self.endpoints.lock().unwrap();
            let Some(live) = endpoints.iter().find(|l| l.stored.spec.serves(id)) else { return };
            let spec = &live.stored.spec;
            if spec.kind != EndpointKind::Gateway || !spec.fallback {
                return;
            }
            spec.base_url.clone()
        };
        let (up, changed) = self.gateway_health.check(&base).await;
        if changed {
            tracing::info!(provider = %id.0, "the roll gateway at {base} {}", if up { "answers again" } else { "doesn't answer: its providers use their own logins" });
            self.configure_providers();
            let me = self.me.clone();
            tokio::spawn(async move {
                if let Some(host) = me.upgrade() {
                    host.publish_info().await;
                }
            });
        }
    }

    /// The attribution variables for a session of `thread` (in `project`) on `id`, when that
    /// provider's requests go through a gateway now ([`gateway::attribution_env`]).
    pub(crate) fn gateway_attribution(&self, id: &ProviderId, env: &[(String, String)], thread: &ThreadId, project: &ProjectId) -> Vec<(String, String)> {
        let endpoints = self.endpoints.lock().unwrap();
        match endpoints.iter().find(|l| l.stored.spec.serves(id)) {
            Some(live) if live.stored.spec.kind == EndpointKind::Gateway && !self.fallen_back(live) => gateway::attribution_env(env, thread.as_str(), project.as_str()),
            _ => Vec::new(),
        }
    }
}

/// Says `hello` again whenever the gateway's run state changes (a listener failing, a retry
/// binding it).
fn spawn_watcher(me: Weak<Host>, gw: model_gateway::Gateway) -> tokio::task::AbortHandle {
    let mut rx = gw.watch_run();
    tokio::spawn(async move {
        while rx.changed().await.is_ok() {
            let Some(host) = me.upgrade() else { return };
            host.publish_info().await;
        }
    })
    .abort_handle()
}

fn describe(run: &GatewayRun) -> String {
    match run {
        GatewayRun::Stopped => "stopped".into(),
        GatewayRun::Running { addresses, .. } => format!("listening on {}", addresses.join(", ")),
        GatewayRun::Failed { error, .. } => format!("not serving: {error}"),
    }
}
