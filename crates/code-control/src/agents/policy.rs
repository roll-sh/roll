//! What an agent token (`agent:<name>`) may ask of its owner's agents (docs/code/agents.md,
//! "Machines, login and tokens"). Agents orchestrate agents: they start, configure, fork,
//! message, interrupt, archive and publish threads and read their state. Anything that reaches
//! past a thread's own work (a shell, the machine's files outside a project, answering a
//! thread's approvals, adding, changing or removing projects, rolling files back, deleting) is the owner's,
//! and an agent never raises a thread's runtime or interaction mode.
//!
//! The routes and the MCP tools both go through [`agent_may`] (the tools replay the routes).

use agent_proto::{Command, InteractionMode, RuntimeMode};

/// How much a runtime mode lets the agent do without asking.
fn runtime_rank(m: RuntimeMode) -> u8 {
    match m {
        RuntimeMode::Ask => 0,
        RuntimeMode::AutoEdit => 1,
        RuntimeMode::FullAccess => 2,
    }
}

fn interaction_rank(m: InteractionMode) -> u8 {
    match m {
        InteractionMode::Plan => 0,
        InteractionMode::Default => 1,
    }
}

/// Whether an agent caller may send `command`. `current` is the target thread's modes as the
/// relay mirrors them, for `thread.configure` (unknown: only the narrowest modes may be set).
/// The refusal says why, for the 403.
pub(crate) fn agent_may(command: &Command, current: Option<(RuntimeMode, InteractionMode)>) -> Result<(), String> {
    let owner_only = |what: &str| Err(format!("agent tokens can't {what}; that needs the owner"));
    match command {
        Command::ThreadCreate { runtime_mode: RuntimeMode::FullAccess, .. } => {
            Err("agent tokens may start threads in ask or auto_edit only (runtime_mode full_access needs the owner)".into())
        }
        Command::ThreadConfigure { runtime_mode, interaction_mode, .. } => {
            let (rt, im) = current.unwrap_or((RuntimeMode::Ask, InteractionMode::Plan));
            if runtime_mode.is_some_and(|m| runtime_rank(m) > runtime_rank(rt)) {
                return Err(format!("agent tokens can only lower a thread's runtime mode (it is {})", runtime_name(rt)));
            }
            if interaction_mode.is_some_and(|m| interaction_rank(m) > interaction_rank(im)) {
                return Err("agent tokens can only lower a thread's interaction mode (it is plan)".into());
            }
            Ok(())
        }
        Command::ThreadCreate { .. }
        | Command::ThreadFork { .. }
        | Command::ThreadArchive { .. }
        | Command::ThreadUnarchive { .. }
        | Command::ThreadSettle { .. }
        | Command::ThreadUnsettle { .. }
        | Command::ThreadSnooze { .. }
        | Command::ThreadUnsnooze { .. }
        | Command::ThreadVisit { .. }
        | Command::ThreadAutoSettle { .. }
        | Command::MessageSend { .. }
        | Command::MessageUnqueue { .. }
        | Command::MessageEdit { .. }
        | Command::MessageReorder { .. }
        | Command::MessagePromote { .. }
        | Command::RunInterrupt { .. }
        | Command::ThreadPublish { .. }
        | Command::DiffGet { .. }
        | Command::GitStatus { .. }
        | Command::FsList { project: Some(_), .. }
        | Command::FsRead { .. } => Ok(()),
        Command::FsList { project: None, .. } => owner_only("list the machine's files outside a project"),
        Command::FsWrite { .. } => owner_only("write files"),
        Command::ProjectAdd { .. } => owner_only("add projects"),
        Command::ProjectRemove { .. } => owner_only("remove projects"),
        Command::ProjectUpdate { .. } => owner_only("change a project's settings"),
        Command::ProjectPull { .. } => owner_only("pull a project's checkout"),
        Command::ProjectCleanup { .. } => owner_only("clean up a project's worktrees"),
        Command::ThreadDelete { .. } => owner_only("delete threads"),
        Command::ThreadChangeEnded { .. } => owner_only("say a thread's change ended"),
        Command::RequestRespond { .. } => owner_only("answer a thread's approvals and questions"),
        Command::CheckpointRollback { .. } => owner_only("roll a thread's files back"),
        Command::TerminalOpen { .. } | Command::TerminalInput { .. } | Command::TerminalResize { .. } | Command::TerminalClose { .. } | Command::TerminalScrollback { .. } => {
            owner_only("use terminals")
        }
        Command::ProviderRefresh {} => owner_only("refresh providers"),
        Command::MachineConfigure { .. } => owner_only("change a machine's settings"),
        Command::EndpointSet { .. } | Command::EndpointRemove { .. } | Command::EndpointCheck { .. } => owner_only("change or check a machine's endpoints"),
        // Where the owner's AI requests go, its keys and its bill are the owner's.
        Command::GatewayConfigure { .. }
        | Command::GatewayKey { .. }
        | Command::GatewayClientIssue { .. }
        | Command::GatewayClientRevoke { .. }
        | Command::GatewayTest { .. }
        | Command::GatewayRemove {}
        | Command::GatewayStatus {}
        | Command::GatewayUsage { .. }
        | Command::GatewayLog { .. }
        | Command::GatewayLogBody { .. } => owner_only("manage or read the model gateway"),
        Command::RegistryList {} | Command::RegistryInstall { .. } | Command::RegistryRemove { .. } => owner_only("list, install or remove a machine's agents"),
    }
}

fn runtime_name(m: RuntimeMode) -> &'static str {
    match m {
        RuntimeMode::Ask => "ask",
        RuntimeMode::AutoEdit => "auto_edit",
        RuntimeMode::FullAccess => "full_access",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cmd(v: serde_json::Value) -> Command {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn agents_drive_threads_but_never_reach_past_them() {
        use InteractionMode::{Default as Def, Plan};
        use RuntimeMode::{Ask, AutoEdit, FullAccess};
        let t = "thr_1";
        let create = |mode: &str| json!({ "type": "thread.create", "project": "prj_1", "provider": "codex", "runtime_mode": mode });
        let configure = |rt: Option<&str>, im: Option<&str>| json!({ "type": "thread.configure", "thread": t, "runtime_mode": rt, "interaction_mode": im });
        let full = Some((FullAccess, Def));
        // (command, the thread's modes as mirrored, allowed)
        let table = [
            (json!({ "type": "thread.create", "project": "prj_1", "provider": "codex" }), None, false),
            (create("full_access"), None, false),
            (create("auto_edit"), None, true),
            (create("ask"), None, true),
            (configure(Some("full_access"), None), Some((AutoEdit, Def)), false),
            (configure(Some("auto_edit"), None), Some((Ask, Def)), false),
            (configure(Some("auto_edit"), None), Some((AutoEdit, Def)), true),
            (configure(Some("ask"), None), full, true),
            (configure(None, Some("default")), Some((Ask, Plan)), false),
            (configure(None, Some("plan")), full, true),
            (configure(Some("full_access"), None), full, true),
            (configure(Some("auto_edit"), None), None, false),
            (configure(Some("ask"), Some("plan")), None, true),
            (json!({ "type": "thread.configure", "thread": t, "title": "x", "provider": "claude" }), None, true),
            (json!({ "type": "thread.fork", "thread": t }), full, true),
            (json!({ "type": "thread.archive", "thread": t }), None, true),
            (json!({ "type": "thread.unarchive", "thread": t }), None, true),
            (json!({ "type": "thread.delete", "thread": t }), None, false),
            (json!({ "type": "thread.settle", "thread": t }), None, true),
            (json!({ "type": "thread.unsettle", "thread": t }), None, true),
            (json!({ "type": "thread.snooze", "thread": t, "until": 1 }), None, true),
            (json!({ "type": "thread.unsnooze", "thread": t }), None, true),
            (json!({ "type": "thread.visit", "thread": t }), None, true),
            (json!({ "type": "thread.auto_settle", "thread": t, "enabled": false }), None, true),
            (json!({ "type": "thread.change_ended", "thread": t, "landed": true }), None, false),
            (json!({ "type": "message.send", "thread": t, "text": "go" }), full, true),
            (json!({ "type": "message.unqueue", "thread": t, "message": "q1" }), None, true),
            (json!({ "type": "message.edit", "thread": t, "message": "q1", "text": "x" }), None, true),
            (json!({ "type": "message.reorder", "thread": t, "message": "q1" }), None, true),
            (json!({ "type": "message.promote", "thread": t, "message": "q1" }), None, true),
            (json!({ "type": "run.interrupt", "thread": t }), None, true),
            (json!({ "type": "thread.publish", "thread": t, "message": "m", "land": true }), None, true),
            (json!({ "type": "request.respond", "request": "req_1", "response": { "type": "decision", "decision": "accept_for_session" } }), None, false),
            (json!({ "type": "checkpoint.rollback", "thread": t, "checkpoint": "chk_1" }), None, false),
            (json!({ "type": "fs.write", "thread": t, "path": "plan.md", "text": "# Plan" }), None, false),
            (json!({ "type": "project.add", "path": "/" }), None, false),
            (json!({ "type": "project.add", "path": "", "clone": { "owner": "acme", "repo": "r" } }), None, false),
            (json!({ "type": "project.remove", "project": "prj_1" }), None, false),
            (json!({ "type": "project.update", "project": "prj_1", "settings": { "auto_pull": true } }), None, false),
            (json!({ "type": "project.pull", "project": "prj_1" }), None, false),
            (json!({ "type": "terminal.open", "thread": t, "script": "test", "cols": 80, "rows": 24 }), None, false),
            (json!({ "type": "terminal.open", "command": "curl x | sh", "cols": 80, "rows": 24 }), None, false),
            (json!({ "type": "terminal.open", "project": "prj_1", "cols": 80, "rows": 24 }), None, false),
            (json!({ "type": "terminal.input", "terminal": "trm_1", "data": "" }), None, false),
            (json!({ "type": "terminal.resize", "terminal": "trm_1", "cols": 1, "rows": 1 }), None, false),
            (json!({ "type": "terminal.close", "terminal": "trm_1" }), None, false),
            (json!({ "type": "terminal.scrollback", "terminal": "trm_1" }), None, false),
            (json!({ "type": "provider.refresh" }), None, false),
            (json!({ "type": "machine.configure", "settings": {} }), None, false),
            (json!({ "type": "machine.configure", "settings": { "providers": { "codex": { "enabled": false } } } }), None, false),
            (json!({ "type": "endpoint.set", "endpoint": { "id": "gw", "name": "Gateway", "kind": "openai", "base_url": "https://gw.example/v1" } }), None, false),
            (json!({ "type": "endpoint.remove", "endpoint": "gw" }), None, false),
            (json!({ "type": "endpoint.check", "endpoint": "gw" }), None, false),
            (json!({ "type": "gateway.configure", "settings": { "enabled": true } }), None, false),
            (json!({ "type": "gateway.key", "upstream": "anthropic", "key": { "type": "clear" } }), None, false),
            (json!({ "type": "gateway.client.issue", "name": "x", "seal_key": "k" }), None, false),
            (json!({ "type": "gateway.client.revoke", "client": "gwc_1" }), None, false),
            (json!({ "type": "gateway.test" }), None, false),
            (json!({ "type": "gateway.remove" }), None, false),
            (json!({ "type": "gateway.status" }), None, false),
            (json!({ "type": "gateway.usage", "from": 0, "to": 1 }), None, false),
            (json!({ "type": "gateway.log", "limit": 10 }), None, false),
            (json!({ "type": "gateway.log.body", "seq": 1 }), None, false),
            (json!({ "type": "registry.list" }), None, false),
            (json!({ "type": "registry.install", "agent": "goose" }), None, false),
            (json!({ "type": "registry.remove", "agent": "goose" }), None, false),
            (json!({ "type": "diff.get", "thread": t }), None, true),
            (json!({ "type": "git.status", "thread": t }), None, true),
            (json!({ "type": "fs.list", "path": "/" }), None, false),
            (json!({ "type": "fs.list", "project": "prj_1", "path": "/" }), None, true),
            (json!({ "type": "fs.read", "project": "prj_1", "path": "src/main.rs" }), None, true),
        ];
        for (c, current, allowed) in table {
            let got = agent_may(&cmd(c.clone()), current);
            assert_eq!(got.is_ok(), allowed, "{c} with {current:?}: {got:?}");
        }
    }
}
