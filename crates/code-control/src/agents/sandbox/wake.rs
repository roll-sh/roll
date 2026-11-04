//! Sandboxes run only while their agents work (docs/code/agents.md, "Sleep and wake"): billing is
//! per running hour, so a sandbox nobody uses goes to sleep, and the next command wakes it.
//!
//! - **Sleep.** At each look the owner's reconciler asks [`Code::goes_to_sleep`] about a running
//!   sandbox. Its machine is busy while a thread on it is starting or running in the mirror (a
//!   thread only waiting on its user isn't: it was asked to wait). Activity the relay sees go by
//!   (a command for it, terminal output) is noted in memory ([`Agents::touch`]) and folded into the
//!   durable `last_active_at` at each look; a wake sets it too. Once nothing happened for
//!   `idle_stop`, the sandbox's desired state becomes `asleep` and the reconciler stops it, which
//!   ends its metering. A reconciler that just started on this node (a restart, a lease move)
//!   doesn't know what the last one saw since the last durable write, so nothing sleeps sooner
//!   than one window after it started.
//! - **Wake.** A command to a sandbox machine ([`Code::machine_command`]: the `/commands` route,
//!   the MCP tools through it, and new projects) that is asleep, stopped by its owner or on its way
//!   down sets it running (`starting` on the stream), kicks the reconciler and holds the command
//!   until a link newer than the one the stop left behind attaches, at most `wake_timeout`. Then
//!   the command goes out once, like any other: the daemon replies only after it has sent every
//!   record up to its newest `seq`, its replay included, so a client that has the reply finds the
//!   machine's records. A command that times out answers `machine_offline` saying the sandbox is
//!   still waking; sending it again with the same id is safe. A deleted or deleting sandbox never
//!   wakes.
//! - **The race.** A command notes itself before it reads the desired state, and the reconciler
//!   puts a sandbox to sleep only if `last_active_at` hasn't moved, then reads the notes again
//!   (and undoes it if one came in): either the command sees it asleep and wakes it, or the
//!   reconciler sees the command. The wake takes the sandbox's lock, so a stop in progress
//!   finishes before it, and the link that stop leaves behind is never used.
//!
//! [`Agents::touch`]: crate::agents::Agents

use super::Desired;
use super::store::{self, SandboxRow};
use crate::Code;
use crate::agents::error;
use crate::agents::store::{self as mirror, MachineRow};
use agent_proto::{CodeGrant, Command, ErrorCode, Reply, SandboxStatus};

impl Code {
    /// Whether a running sandbox goes to sleep now (and, if so, it's `asleep` already). Records
    /// the activity seen since the last look either way.
    pub(super) async fn goes_to_sleep(&self, s: &SandboxRow) -> bool {
        let window = self.config.sandboxes.idle_stop;
        if window.is_zero() {
            return false;
        }
        let Some(since) = self.sandboxes.reconcilers.lock().get(&s.owner_id).map(|r| r.since) else { return false };
        let db = self.db.sql();
        let now = code_sql::now_ms();
        // Offline, the mirror's statuses are the daemon's last word, not work going on.
        let busy = self.agents.online(&s.machine_id) && mirror::machine_busy(db, &s.machine_id).unwrap_or(true);
        let durable = s.last_active_at.unwrap_or(s.created_at);
        let last = if busy { now } else { durable.max(self.agents.touched(&s.machine_id).unwrap_or(0)) };
        if last > durable
            && let Err(e) = store::active(db, &s.id, last)
        {
            tracing::warn!(sandbox = %s.id, error = %e, "recording a sandbox's activity");
        }
        if now - last.max(since) < window.as_millis() as i64 {
            return false;
        }
        match store::sleep(db, &s.id, last) {
            Ok(true) => {}
            Ok(false) => return false,
            Err(e) => {
                tracing::warn!(sandbox = %s.id, error = %e, "putting a sandbox to sleep");
                return false;
            }
        }
        // A command that came in meanwhile keeps it up.
        if self.agents.touched(&s.machine_id).is_some_and(|t| t > last) {
            if let Err(e) = store::unsleep(db, &s.id) {
                tracing::warn!(sandbox = %s.id, error = %e, "keeping a sandbox up");
            }
            return false;
        }
        tracing::info!(sandbox = %s.id, idle_s = (now - last) / 1000, "a sandbox goes to sleep");
        true
    }

    /// Send a command to one of the owner's machines and wait for its reply, waking it first if
    /// it's a sandbox that isn't running.
    pub(crate) async fn machine_command(&self, m: &MachineRow, id: String, by: String, command: Command, grant: Option<CodeGrant>) -> Reply {
        if m.sandbox.is_some()
            && self.sandboxes.backend.is_some()
            && let Some(reply) = self.wake(m).await
        {
            return reply;
        }
        self.agents.command(&m.id, id, by, command, grant, self.agents.command_timeout()).await
    }

    /// Make sure a sandbox machine is up for a command: at once when it runs and is connected;
    /// else set it running and wait for its daemon's link. `Some` is the reply to give instead.
    async fn wake(&self, m: &MachineRow) -> Option<Reply> {
        let offline = |why: &str| Some(error(ErrorCode::MachineOffline, why));
        // Noted before the desired state is read (the module doc's race).
        self.agents.touch(&m.id);
        let db = self.db.sql();
        let s = match store::by_machine(db, &m.id) {
            Ok(Some(s)) => s,
            Ok(None) => return None,
            Err(e) => {
                tracing::warn!(machine = %m.id, error = %e, "reading a machine's sandbox");
                return None;
            }
        };
        let online = self.agents.online(&m.id);
        if s.desired == Desired::Running && online {
            return None;
        }
        if let Some(why) = unwakeable(&s) {
            return if online { None } else { offline(&why) };
        }
        let stale = {
            // A look in progress (a stop) finishes first.
            let lock = self.sandboxes.lock(&s.id);
            let _one = lock.lock().await;
            let s = match store::sandbox(db, &s.id) {
                Ok(Some(s)) => s,
                Ok(None) => return offline("the sandbox was deleted"),
                Err(e) => {
                    tracing::warn!(sandbox = %s.id, error = %e, "reading a sandbox to wake");
                    return offline("the sandbox couldn't be woken; try again");
                }
            };
            if let Some(why) = unwakeable(&s) {
                return offline(&why);
            }
            if s.desired != Desired::Running || s.status == SandboxStatus::Stopped {
                let hub = self.agents.hub(&s.owner_id);
                let _order = hub.order.lock().await;
                match store::set_desired(db, &s.owner_id, &s.id, Desired::Running) {
                    Ok(Some((_, Some(machine)))) => self.publish_machine(&machine),
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(sandbox = %s.id, error = %e, "waking a sandbox");
                        return offline("the sandbox couldn't be woken; try again");
                    }
                }
                tracing::info!(sandbox = %s.id, was = s.desired.as_str(), "waking a sandbox for a command");
            }
            self.sandboxes.stopped_links.lock().remove(&s.machine_id).flatten()
        };
        self.kick_sandboxes(&s.owner_id);
        let wait = self.config.sandboxes.wake_timeout;
        if self.agents.linked(&m.id, stale, tokio::time::Instant::now() + wait).await {
            return None;
        }
        offline(&format!("the sandbox is still waking up (it didn't connect within {} s); send the command again with the same id", wait.as_secs()))
    }
}

/// Why a sandbox can't be woken, if it can't: it's being deleted, or it failed.
fn unwakeable(s: &SandboxRow) -> Option<String> {
    if s.deleted_at.is_some() || s.desired == Desired::Deleted {
        return Some("the sandbox is being deleted".into());
    }
    if s.status == SandboxStatus::Failed {
        return Some(match &s.error {
            Some(e) => format!("the sandbox failed: {e}"),
            None => "the sandbox failed".into(),
        });
    }
    None
}
