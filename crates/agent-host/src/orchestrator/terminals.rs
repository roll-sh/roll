//! `terminal.*`: PTYs as records. Output goes out as `Delta::Terminal` (base64) and into a
//! 256 KiB ring for `terminal.scrollback`; an exit updates the record. Closing a terminal hangs
//! up its session and removes the record; one that exits on its own stays, with its exit code,
//! until it is closed.

use super::{Fail, Host, Res, now, suffix};
use crate::terminal::{self, Pty, Ring, Spawn};
use agent_proto::*;
use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Mutex as AsyncMutex;

/// The most one `terminal.input` may carry (base64).
const INPUT_CAP: usize = 1 << 20;

pub(crate) struct LiveTerminal {
    pty: Pty,
    ring: Arc<Mutex<Ring>>,
    /// Keeps inputs in order while a write waits for the terminal.
    input: AsyncMutex<()>,
}

impl Host {
    fn terminal_record(&self, id: &TerminalId) -> Res<Terminal> {
        match self.store.get(EntityKind::Terminal, id.as_str())? {
            Some(Entity::Terminal(t)) => Ok(t),
            _ => Err(Fail::NotFound(format!("no terminal {id}"))),
        }
    }

    fn live_terminal(&self, id: &TerminalId) -> Option<Arc<LiveTerminal>> {
        self.terminals.lock().unwrap().get(id).cloned()
    }

    pub(crate) async fn terminal_open(
        &self,
        project: Option<ProjectId>,
        thread: Option<ThreadId>,
        command: Option<String>,
        script: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Res<Reply> {
        let (project, cwd, worktree) = match (&thread, &project) {
            (Some(thread), _) => {
                let mut t = self.stored_thread(thread)?;
                if t.worktree.as_ref().is_some_and(|w| w.removed_at.is_some()) {
                    // A terminal in a cleaned-up thread puts its worktree back first.
                    let mut rt = self.lock(thread).await?;
                    self.ensure_worktree(&mut rt).await?;
                    t = rt.thread.clone();
                }
                (Some(t.project), t.cwd, t.worktree.map(|w| w.path))
            }
            (None, Some(project)) => (Some(project.clone()), self.project(project)?.path, None),
            (None, None) => {
                let home = crate::config::user_home().unwrap_or_else(|| PathBuf::from("/"));
                (None, home.to_string_lossy().into_owned(), None)
            }
        };
        let command = command.map(|c| c.trim().to_string()).filter(|c| !c.is_empty());
        // A project script (T3's `projectScripts.ts`): its command, told where it runs.
        let (command, env, name) = match script {
            None => (command, Vec::new(), None),
            Some(_) if command.is_some() => return Err(Fail::Invalid("give a command or a script, not both".into())),
            Some(id) => {
                let p = self.project(project.as_ref().ok_or_else(|| Fail::Invalid("a script needs a project or a thread".into()))?)?;
                let s = p.settings.scripts.into_iter().find(|s| s.id == id).ok_or_else(|| Fail::NotFound(format!("no script {id} in {}", p.name)))?;
                let mut env = vec![("ROLL_PROJECT_ROOT", p.path)];
                env.extend(worktree.map(|w| ("ROLL_WORKTREE_PATH", w)));
                (Some(s.command), env, Some(s.name))
            }
        };
        let spawned = terminal::spawn(&Spawn { cwd: cwd.as_ref(), command: command.as_deref(), env: &env, cols, rows })
            .map_err(|e| Fail::Invalid(format!("starting a terminal in {cwd}: {e}")))?;
        let title = name.or_else(|| command.clone()).unwrap_or_else(|| {
            let shell = terminal::shell();
            shell.rsplit('/').next().unwrap_or(&shell).to_string()
        });
        let record = Terminal {
            id: TerminalId::from_suffix(&suffix()),
            project,
            thread,
            cwd,
            title,
            status: TerminalStatus::Running,
            exit_code: None,
            cols,
            rows,
            created_at: now(),
        };
        let id = record.id.clone();
        // What a thread's terminal does to its files counts in its changes.
        let owner = record.thread.clone();
        self.put(vec![Entity::Terminal(record)])?;

        let ring = Arc::new(Mutex::new(Ring::default()));
        let live = Arc::new(LiveTerminal { pty: spawned.pty.clone_handle(), ring: ring.clone(), input: AsyncMutex::new(()) });
        self.terminals.lock().unwrap().insert(id.clone(), live);

        let mut output = spawned.output;
        let host = self.me.clone();
        let out_id = id.clone();
        let out_owner = owner.clone();
        let pump = tokio::spawn(async move {
            while let Some(chunk) = output.recv().await {
                ring.lock().unwrap().push(&chunk);
                let Some(host) = host.upgrade() else { return };
                host.delta(Delta::Terminal { terminal: out_id.clone(), data: B64.encode(&chunk) });
                if let Some(thread) = &out_owner {
                    host.touch_diff(thread);
                }
            }
        });
        let host = self.me.clone();
        let mut child = spawned.child;
        let exit_id = id.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // Let the last output through (a background job may hold the terminal open: don't
            // wait for it forever).
            let _ = tokio::time::timeout(Duration::from_secs(1), pump).await;
            let Some(host) = host.upgrade() else { return };
            host.terminals.lock().unwrap().remove(&exit_id);
            if let Some(thread) = &owner {
                host.touch_diff(thread);
            }
            let code = status.ok().and_then(|s| s.code().or_else(|| s.signal().map(|n| 128 + n)));
            if let Ok(mut record) = host.terminal_record(&exit_id) {
                record.status = TerminalStatus::Exited;
                record.exit_code = code;
                if let Err(e) = host.put(vec![Entity::Terminal(record)]) {
                    tracing::warn!("recording the exit of {exit_id}: {e:#}");
                }
            }
            // A provider login runs in a terminal (the card's "Log in to …", or typed in a
            // shell): when one ends while a provider is logged out, check again, so the machine
            // shows the login without anyone asking for a refresh.
            if host.logged_out() {
                host.probe_providers();
                host.publish_info().await;
            }
        });
        Ok(Reply::Created { id: id.0 })
    }

    pub(crate) async fn terminal_input(&self, id: &TerminalId, data: &str) -> Res<Reply> {
        if data.len() > INPUT_CAP {
            return Err(Fail::Invalid("too much input at once".into()));
        }
        let bytes = B64.decode(data).map_err(|_| Fail::Invalid("the input is not base64".into()))?;
        let Some(live) = self.live_terminal(id) else {
            self.terminal_record(id)?;
            return Err(Fail::Conflict(format!("terminal {id} has exited")));
        };
        let _order = live.input.lock().await;
        let pty = live.pty.clone_handle();
        tokio::task::spawn_blocking(move || pty.write(&bytes))
            .await
            .map_err(|e| Fail::Internal(e.into()))?
            .map_err(|e| Fail::Conflict(format!("writing to terminal {id}: {e}")))?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn terminal_resize(&self, id: &TerminalId, cols: u16, rows: u16) -> Res<Reply> {
        let mut record = self.terminal_record(id)?;
        if let Some(live) = self.live_terminal(id) {
            live.pty.resize(cols, rows).map_err(|e| Fail::Conflict(format!("resizing terminal {id}: {e}")))?;
        }
        record.cols = cols;
        record.rows = rows;
        self.put(vec![Entity::Terminal(record)])?;
        Ok(Reply::Ok)
    }

    pub(crate) async fn terminal_close(&self, id: &TerminalId) -> Res<Reply> {
        let record = self.terminal_record(id)?;
        self.hang_up(id);
        self.put(vec![Entity::Removed(Removed { entity: EntityKind::Terminal, id: id.0.clone(), thread: record.thread })])?;
        Ok(Reply::Ok)
    }

    /// Hangs up a running terminal, and kills it if it's still there two seconds later.
    pub(crate) fn hang_up(&self, id: &TerminalId) {
        let Some(live) = self.live_terminal(id) else { return };
        live.pty.hangup();
        let host = self.me.clone();
        let id = id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            let Some(host) = host.upgrade() else { return };
            if let Some(live) = host.live_terminal(&id) {
                live.pty.kill();
            }
        });
    }

    pub(crate) async fn terminal_scrollback(&self, id: &TerminalId) -> Res<Reply> {
        let data = match self.live_terminal(id) {
            Some(live) => live.ring.lock().unwrap().bytes(),
            None => {
                self.terminal_record(id)?;
                Vec::new()
            }
        };
        Ok(Reply::Scrollback { data: B64.encode(data) })
    }

    /// Closes the terminals `pred` picks: hangs them up and returns their tombstones.
    pub(crate) fn close_terminals(&self, pred: impl Fn(&Terminal) -> bool) -> anyhow::Result<Vec<Entity>> {
        let mut tombstones = Vec::new();
        for entity in self.store.list(EntityKind::Terminal)? {
            if let Entity::Terminal(t) = entity
                && pred(&t)
            {
                self.hang_up(&t.id);
                tombstones.push(Entity::Removed(Removed { entity: EntityKind::Terminal, id: t.id.0, thread: t.thread }));
            }
        }
        Ok(tombstones)
    }
}
