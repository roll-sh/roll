//! Endpoints whose key is a roll secrets reference (docs/code/agents.md, "Endpoints"): an
//! endpoint's `key_ref` may be `roll://ENV[/folder]/KEY` instead of a key sealed in the browser.
//!
//! - **Bound to whoever sets it.** `endpoint.set` with one is bound by roll secrets to the
//!   command's sender (`Secrets::bind_ref` for `agent:<machine>`, through the project that
//!   stands for the machine's owner: they must be able to reveal it). The maker is kept here
//!   (`agent_endpoint_refs`), where only the relay writes; the reference rides to the daemon in
//!   the endpoint as before.
//! - **Sent sealed, as the browser would.** While a machine's link is up, [`follow`] reads each
//!   such endpoint its `hello` reports, as the maker may now (`digest_as`, then `resolve_as` when
//!   it moved), and sends the value sealed to the daemon's `seal_key` (`endpoint.key`), or why it
//!   can't be read. It does so on the first `hello`, whenever one reports a new reference, and when
//!   a periodic check finds the value moved or the maker unable to read it. The relay keeps no
//!   value: it seals what roll secrets answered and drops it; nothing is logged or stored.
//! - **Failing closed.** A refusal (the maker removed or demoted, the key gone) is sent too: the
//!   daemon drops the key it held, and the endpoint's `error` says why.

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
    endpoint_id TEXT NOT NULL,
    -- roll://ENV[/folder]/KEY, as the endpoint's key_ref.
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

/// `endpoint.set` with a `roll://` key: roll secrets checks the sender may choose it for the
/// machine, and the maker it answers is kept for the machine's endpoint.
pub(super) async fn bind(code: &Code, o: &Owner, m: &MachineRow, endpoint: &str, reference: &str, asker: &SecretsAsker) -> Result<()> {
    let secrets = code.secret_env.get().ok_or_else(|| Error::Unprocessable("roll:// keys need roll secrets on this server".into()))?;
    let maker = secrets.bind_machine_key(&o.name, &m.name, reference, asker).await?;
    code.db.sql().execute(
        "INSERT INTO agent_endpoint_refs (machine_id, endpoint_id, reference, maker, updated_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT (machine_id, endpoint_id) DO UPDATE SET reference = excluded.reference, maker = excluded.maker, updated_at = excluded.updated_at",
        params![m.id, endpoint, reference, maker, code_sql::now_ms()],
    )?;
    Ok(())
}

/// The endpoint no longer has a `roll://` key (set with another key, or removed).
pub(super) fn unbind(code: &Code, machine: &str, endpoint: &str) -> Result<()> {
    code.db.sql().execute("DELETE FROM agent_endpoint_refs WHERE machine_id = ? AND endpoint_id = ?", params![machine, endpoint])?;
    Ok(())
}

/// The reference bound for a machine's endpoint, and its maker.
fn bound(code: &Code, machine: &str, endpoint: &str) -> Result<Option<(String, String)>> {
    Ok(code
        .db
        .sql()
        .query_row("SELECT reference, maker FROM agent_endpoint_refs WHERE machine_id = ? AND endpoint_id = ?", params![machine, endpoint], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?)
}

/// What was last sent for an endpoint: its reference, and the digest of the value sent or the
/// reason given. Kept per link, so a new link (a restarted daemon) is sent everything again.
#[derive(Clone, PartialEq, Eq)]
enum Sent {
    Value(String, String),
    Refused(String, String),
}

/// While `machine`'s link is up (until `info`'s sender is dropped): keep its daemon's `roll://`
/// endpoint keys as roll secrets answers now, after every `hello` and every [`EVERY`].
pub(super) fn follow(code: Arc<Code>, machine: MachineRow, mut info: tokio::sync::watch::Receiver<Option<MachineInfo>>) {
    tokio::spawn(async move {
        let mut sent: HashMap<String, Sent> = HashMap::new();
        loop {
            let now = info.borrow_and_update().clone();
            if let Some(i) = now {
                keep(&code, &machine, &i, &mut sent).await;
            }
            tokio::select! {
                changed = info.changed() => if changed.is_err() { return },
                _ = tokio::time::sleep(EVERY) => {}
            }
        }
    });
}

async fn keep(code: &Code, machine: &MachineRow, info: &MachineInfo, sent: &mut HashMap<String, Sent>) {
    let wanted: Vec<(String, String)> = info
        .settings
        .iter()
        .flat_map(|s| s.endpoints.iter())
        .filter_map(|e| Some((e.spec.id.clone(), e.spec.roll_ref()?.to_string())))
        .collect();
    sent.retain(|id, _| wanted.iter().any(|(w, _)| w == id));
    let (Some(seal_key), false) = (info.seal_key.as_deref(), wanted.is_empty()) else { return };
    let owner = match code.db.owner_by_id(&machine.owner_id) {
        Ok(Some(o)) => o,
        _ => return,
    };
    for (endpoint, reference) in wanted {
        let (now, key) = read(code, &owner, machine, &endpoint, &reference, seal_key, sent.get(&endpoint)).await;
        let Some(key) = key else { continue };
        let id = format!("refkey-{endpoint}-{}", super::api::random_hex(6));
        match code.machine_command(machine, id, "roll".into(), Command::EndpointKey { endpoint: endpoint.clone(), key }, None).await {
            // Tried again at the next `hello` or check.
            Reply::Error(e) => tracing::info!(machine = %machine.id, %endpoint, error = %e.message, "sending an endpoint's roll:// key"),
            _ => {
                sent.insert(endpoint, now);
            }
        }
    }
}

/// What to send for one endpoint now (`None`: what was sent still holds).
async fn read(code: &Code, owner: &Owner, machine: &MachineRow, endpoint: &str, reference: &str, seal_key: &str, was: Option<&Sent>) -> (Sent, Option<RefKey>) {
    let refused = |reason: String| {
        let now = Sent::Refused(reference.to_string(), reason.clone());
        let key = (was != Some(&now)).then_some(RefKey::Refused { reason });
        (now, key)
    };
    let Some(secrets) = code.secret_env.get() else { return refused("roll secrets isn't on this server".into()) };
    let maker = match bound(code, &machine.id, endpoint) {
        Ok(Some((r, maker))) if r == reference => maker,
        Ok(_) => return refused(format!("nobody roll knows chose {reference} for this endpoint: set its key again from roll")),
        Err(e) => return refused(format!("roll couldn't read who chose {reference}: {e}")),
    };
    let digest = match secrets.machine_key_digest(&owner.name, &machine.name, reference, &maker).await {
        Ok(d) => d,
        Err(e) => return refused(format!("{e:#}")),
    };
    if was == Some(&Sent::Value(reference.to_string(), digest.clone())) {
        return (Sent::Value(reference.to_string(), digest), None);
    }
    let value = match secrets.machine_key(&owner.name, &machine.name, reference, &maker).await {
        Ok(v) => v,
        Err(e) => return refused(format!("{e:#}")),
    };
    match agent_proto::seal::seal_with(seal_key, &SealedSecret::ref_context(endpoint), &value.0) {
        Ok(sealed) => (Sent::Value(reference.to_string(), digest), Some(RefKey::Set { sealed })),
        Err(e) => refused(format!("couldn't seal the key to this machine: {e}")),
    }
}
