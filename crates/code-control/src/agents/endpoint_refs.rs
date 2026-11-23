//! Machine keys that are roll secrets references (docs/code/agents.md, "Endpoints";
//! docs/code/gateway.md, "Upstreams"): an endpoint's `key_ref`, or an upstream's of the model
//! gateway a machine hosts, may be `roll://ENV[/folder]/KEY` instead of a key sealed in the
//! browser.
//!
//! - **Bound to whoever sets it.** `endpoint.set` or `gateway.configure` with one is bound by roll
//!   secrets to the command's sender (`Secrets::bind_ref` for `agent:<machine>`, through the
//!   project that stands for the machine's owner: they must be able to reveal it). The maker is
//!   kept here (`agent_endpoint_refs`), where only the relay writes; the reference rides to the
//!   daemon in the endpoint or the gateway's settings as before.
//! - **Sent sealed, as the browser would.** While a machine's link is up, [`follow`] reads each
//!   such key its `hello` reports, as the maker may now (`digest_as`, then `resolve_as` when it
//!   moved), and sends the value sealed to the daemon's `seal_key` (`endpoint.key`, or
//!   `gateway.upstream_key` for an upstream, each with its own context), or why it can't be read.
//!   It does so on the first `hello`, whenever one reports a new reference, and when a periodic
//!   check finds the value moved or the maker unable to read it. The relay keeps no value: it
//!   seals what roll secrets answered and drops it; nothing is logged or stored.
//! - **Failing closed.** A refusal (the maker removed or demoted, the key gone) is sent too: the
//!   daemon drops the key it held, and the endpoint's `error` (an upstream's status) says why. An
//!   upstream's refusal also bars its provider in the gateway: no other key of it is tried.
//! - **Only on a refusal.** A refusal is roll secrets' [`crate::Refused`], or the slot's binding
//!   gone from `agent_endpoint_refs`. Anything else (roll secrets starting after a restart, a
//!   database that didn't answer) sends nothing: the daemon keeps what it holds, and the slot is
//!   asked again soon ([`RETRY`], doubling up to [`EVERY`]).

use super::store::MachineRow;
use crate::model::Owner;
use crate::{Code, Error, Result, SecretsAsker};
use agent_proto::{Command, MachineInfo, RefKey, Reply, SealedSecret};
use code_sql::{OptionalExtension, params};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS agent_endpoint_refs (
    machine_id TEXT NOT NULL,
    -- The key's place on the machine: an endpoint's id, or `upstream:<id>` for an upstream of
    -- the gateway it hosts (ids can't hold `:`, so the two never meet).
    endpoint_id TEXT NOT NULL,
    -- roll://ENV[/folder]/KEY, as the key_ref.
    reference TEXT NOT NULL,
    -- Who chose it, as roll secrets returned it (`Maker`): re-checked on every read.
    maker TEXT NOT NULL,
    -- Unix ms.
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (machine_id, endpoint_id)
);
"#;

/// How often a connected machine's `roll://` keys are checked again without a `hello`: a
/// rotation, or a maker who can't read the key any more, reaches the daemon within this.
const EVERY: Duration = Duration::from_secs(60);

/// How soon a slot roll secrets couldn't answer for is asked again (then twice as long, up to
/// [`EVERY`]).
const RETRY: Duration = Duration::from_secs(1);

/// Where a `roll://` key is used on a machine.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(super) enum Slot {
    Endpoint(String),
    /// An upstream of the gateway the machine hosts.
    Upstream(String),
}

impl Slot {
    /// Its row key in `agent_endpoint_refs`.
    fn row(&self) -> String {
        match self {
            Slot::Endpoint(id) => id.clone(),
            Slot::Upstream(id) => format!("{UPSTREAM}{id}"),
        }
    }
}

const UPSTREAM: &str = "upstream:";

/// A `roll://` key set on `m` for `slot`: roll secrets checks the sender may choose it for the
/// machine, and the maker it answers is kept for that slot.
pub(super) async fn bind(code: &Code, o: &Owner, m: &MachineRow, slot: &Slot, reference: &str, asker: &SecretsAsker) -> Result<()> {
    let secrets = code.secret_env.get().ok_or_else(|| Error::Unprocessable("roll:// keys need roll secrets on this server".into()))?;
    let maker = secrets.bind_machine_key(&o.name, &m.name, reference, asker).await?;
    code.db.sql().execute(
        "INSERT INTO agent_endpoint_refs (machine_id, endpoint_id, reference, maker, updated_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (machine_id, endpoint_id) DO UPDATE SET reference = excluded.reference, maker = excluded.maker, updated_at = excluded.updated_at",
        params![m.id, slot.row(), reference, maker, code_sql::now_ms()],
    )?;
    Ok(())
}

/// The slot no longer has a `roll://` key (set with another key, or removed).
pub(super) fn unbind(code: &Code, machine: &str, slot: &Slot) -> Result<()> {
    code.db.sql().execute("DELETE FROM agent_endpoint_refs WHERE machine_id = ? AND endpoint_id = ?", params![machine, slot.row()])?;
    Ok(())
}

/// `gateway.configure` (settings replaced whole) or `gateway.remove` (`None`): every upstream
/// with a `roll://` key is bound to the sender, and every other upstream's row of the machine goes.
pub(super) async fn bind_upstreams(code: &Code, o: &Owner, m: &MachineRow, settings: Option<&agent_proto::GatewaySettings>, asker: &SecretsAsker) -> Result<()> {
    let wanted: Vec<(String, String)> =
        settings.iter().flat_map(|s| s.upstreams.iter()).filter_map(|u| Some((u.id.clone(), u.roll_ref()?.to_string()))).collect();
    for (id, reference) in &wanted {
        bind(code, o, m, &Slot::Upstream(id.clone()), reference, asker).await?;
    }
    let rows: Vec<String> = code.db.sql().query("SELECT endpoint_id FROM agent_endpoint_refs WHERE machine_id = ?", [&m.id], |r| r.get(0))?;
    for row in rows {
        if let Some(id) = row.strip_prefix(UPSTREAM)
            && !wanted.iter().any(|(w, _)| w == id)
        {
            unbind(code, &m.id, &Slot::Upstream(id.to_string()))?;
        }
    }
    Ok(())
}

/// The reference bound for a machine's slot, and its maker.
fn bound(code: &Code, machine: &str, slot: &Slot) -> Result<Option<(String, String)>> {
    Ok(code
        .db
        .sql()
        .query_row("SELECT reference, maker FROM agent_endpoint_refs WHERE machine_id = ? AND endpoint_id = ?", params![machine, slot.row()], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// What was last sent for a slot: its reference, and the digest of the value sent or the reason
/// given. Kept per link, so a new link (a restarted daemon) is sent everything again.
#[derive(Clone, PartialEq, Eq)]
enum Sent {
    Value(String, String),
    Refused(String, String),
}

/// While `machine`'s link is up (until `info`'s sender is dropped): keep its daemon's `roll://`
/// keys as roll secrets answers now, after every `hello` and every [`EVERY`].
pub(super) fn follow(code: Arc<Code>, machine: MachineRow, mut info: tokio::sync::watch::Receiver<Option<MachineInfo>>) {
    tokio::spawn(async move {
        let mut sent: HashMap<Slot, Sent> = HashMap::new();
        let mut retry = Duration::ZERO;
        loop {
            let now = info.borrow_and_update().clone();
            let unsure = match now {
                Some(i) => keep(&code, &machine, &i, &mut sent).await,
                None => false,
            };
            retry = if unsure { (retry * 2).clamp(RETRY, EVERY) } else { Duration::ZERO };
            tokio::select! {
                changed = info.changed() => if changed.is_err() { return },
                _ = tokio::time::sleep(if unsure { retry } else { EVERY }) => {}
            }
        }
    });
}

/// The `roll://` keys a `hello` reports: its endpoints', and its hosted gateway's upstreams'.
fn wanted(info: &MachineInfo) -> Vec<(Slot, String)> {
    let Some(s) = &info.settings else { return Vec::new() };
    let endpoints = s.endpoints.iter().filter_map(|e| Some((Slot::Endpoint(e.spec.id.clone()), e.spec.roll_ref()?.to_string())));
    let upstreams = s.gateway.iter().flat_map(|g| g.settings.upstreams.iter()).filter_map(|u| Some((Slot::Upstream(u.id.clone()), u.roll_ref()?.to_string())));
    endpoints.chain(upstreams).collect()
}

/// Whether a slot is left to ask again soon: roll couldn't tell what to send, or the daemon
/// didn't take it.
async fn keep(code: &Code, machine: &MachineRow, info: &MachineInfo, sent: &mut HashMap<Slot, Sent>) -> bool {
    let wanted = wanted(info);
    sent.retain(|slot, _| wanted.iter().any(|(w, _)| w == slot));
    let (Some(seal_key), false) = (info.seal_key.as_deref(), wanted.is_empty()) else { return false };
    let owner = match code.db.owner_by_id(&machine.owner_id) {
        Ok(Some(o)) => o,
        Ok(None) => return false,
        Err(_) => return true,
    };
    let mut unsure = false;
    for (slot, reference) in wanted {
        let (now, key) = match read(code, &owner, machine, &slot, &reference, seal_key, sent.get(&slot)).await {
            Now::Same => continue,
            Now::Unsure(why) => {
                tracing::info!(machine = %machine.id, slot = %slot.row(), error = %why, "roll couldn't read a roll:// key: the daemon keeps what it holds");
                unsure = true;
                continue;
            }
            Now::Send(now, key) => (now, key),
        };
        let (id, command) = match &slot {
            Slot::Endpoint(e) => (format!("refkey-{e}-{}", super::api::random_hex(6)), Command::EndpointKey { endpoint: e.clone(), key }),
            Slot::Upstream(u) => (format!("refkey-up-{u}-{}", super::api::random_hex(6)), Command::GatewayUpstreamKey { upstream: u.clone(), key }),
        };
        match code.machine_command(machine, id, "roll".into(), command, None).await {
            // Tried again at the next `hello` or check.
            Reply::Error(e) => {
                tracing::info!(machine = %machine.id, slot = %slot.row(), error = %e.message, "sending a roll:// key");
                unsure = true;
            }
            _ => {
                sent.insert(slot, now);
            }
        }
    }
    unsure
}

/// What to do for one slot now.
enum Now {
    /// What was sent still holds.
    Same,
    /// roll couldn't tell (why): nothing is sent, and the daemon keeps what it holds.
    Unsure(String),
    Send(Sent, RefKey),
}

async fn read(code: &Code, owner: &Owner, machine: &MachineRow, slot: &Slot, reference: &str, seal_key: &str, was: Option<&Sent>) -> Now {
    let refused = |reason: String| {
        let now = Sent::Refused(reference.to_string(), reason.clone());
        if was == Some(&now) { Now::Same } else { Now::Send(now, RefKey::Refused { reason }) }
    };
    // Only roll secrets' refusal is one.
    let failed = |e: anyhow::Error| match e.downcast::<crate::Refused>() {
        Ok(r) => refused(r.0),
        Err(e) => Now::Unsure(format!("{e:#}")),
    };
    let Some(secrets) = code.secret_env.get() else { return refused("roll secrets isn't on this server".into()) };
    let maker = match bound(code, &machine.id, slot) {
        Ok(Some((r, maker))) if r == reference => maker,
        Ok(_) => return refused(format!("nobody roll knows chose {reference} for this key: set it again from roll")),
        Err(e) => return Now::Unsure(format!("reading who chose {reference}: {e}")),
    };
    let digest = match secrets.machine_key_digest(&owner.name, &machine.name, reference, &maker).await {
        Ok(d) => d,
        Err(e) => return failed(e),
    };
    if was == Some(&Sent::Value(reference.to_string(), digest.clone())) {
        return Now::Same;
    }
    let value = match secrets.machine_key(&owner.name, &machine.name, reference, &maker).await {
        Ok(v) => v,
        Err(e) => return failed(e),
    };
    let context = match slot {
        Slot::Endpoint(e) => SealedSecret::ref_context(e),
        Slot::Upstream(u) => agent_proto::gateway::upstream_ref_context(u),
    };
    match agent_proto::seal::seal_with(seal_key, &context, &value.0) {
        Ok(sealed) => Now::Send(Sent::Value(reference.to_string(), digest), RefKey::Set { sealed }),
        Err(e) => refused(format!("couldn't seal the key to this machine: {e}")),
    }
}
