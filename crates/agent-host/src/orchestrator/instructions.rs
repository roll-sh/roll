//! What every provider session is told about roll's orchestrator MCP, and what an orchestrator
//! thread is told about its job. Ported from T3 Code's
//! `packages/provider-core/src/server/orchestrationInstructions.ts`: every session that gets the
//! MCP server also gets these instructions (Claude: `--append-system-prompt`; Codex:
//! `additionalContext`; ACP: in front of the first prompt), so an agent knows when to delegate
//! and how to wait. T3 has no orchestrator role; roll adds one for "Orchestrate" on the canvas,
//! where the user's prompt is a goal for agents rather than a task for this one.
//!
//! Each thread's text is fixed for its life (it only depends on what the thread is), so it never
//! breaks a provider's prompt cache.

use agent_proto::{Relation, Thread};

/// Every session (T3's `T3_CODE_ORCHESTRATION_SECTIONS`, trimmed to roll's tools).
const DELEGATION: &str = "## roll orchestration

Ordinary coding tasks need no roll tools.

- Subagents: your own subagent tool is fine for quick help on your provider. Use `delegate_task` (MCP server `roll`) when the user should see and steer the work on roll's canvas, for another provider or model, or for a separate checkout (`worktree: true`).
- Call `orchestrator_capabilities` once, before your first `delegate_task`. A provider or model it doesn't list: say so. Never switch unasked.
- Async tasks (the default) wake you with their result when they finish: end your turn instead of polling (`task_status`, `task_wait`, sleeps). Stop a task you no longer need with `task_cancel`.";

/// A thread `delegate_task` started (T3's \"If you are a subagent or child\" line).
const SUBAGENT: &str = "## You are a subagent

Another agent delegated this task to you through roll. You see only the task, not its conversation: everything you need is in it. Do the work yourself, and start no subagents unless your task says to. End with a short report of what you did, where it is (files, branch) and how you checked it: that report is what the other agent gets back.";

/// An orchestrator thread (`Thread::orchestrator`).
const ORCHESTRATOR: &str = "## You are the orchestrator

The user's message is a goal to get done with other agents, not by yourself. They watch your subagents on roll's canvas, under this thread, and can answer their questions there.

1. Look first: read enough of the project to plan (its layout, conventions, how it builds and tests). Change nothing yet.
2. Plan: split the goal into tasks a fresh agent can finish alone, each with a clear end. Tell the user the plan in a few lines.
3. Delegate: `orchestrator_capabilities` once, then one `delegate_task` per task (async), with a short `title`. A task is the whole brief: the goal, the files and facts it needs, what done means and how to check it; the subagent sees nothing of this conversation. Give tasks that edit files `worktree: true`, so parallel work never collides. Start independent tasks together, within the limits `orchestrator_capabilities` reports; start a task that depends on another when that one is done.
4. Then end your turn. Each result wakes you: check it (read the diff or run the tests when it matters), follow up with `task_send` or delegate a fix, and start what comes next.
5. When everything is done, sum it up for the user: what each task did, where its work is (branch or worktree), and what is left open.

Do small glue work yourself (a one-line fix, putting results together); delegate the rest.";

/// The instructions for a session of `thread`.
pub(crate) fn for_thread(thread: &Thread) -> String {
    let role = if thread.orchestrator {
        Some(ORCHESTRATOR)
    } else if thread.lineage.as_ref().is_some_and(|l| l.relation == Relation::Subagent) {
        Some(SUBAGENT)
    } else {
        None
    };
    match role {
        Some(role) => format!("{DELEGATION}\n\n{role}"),
        None => DELEGATION.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_fit_one_codex_context_entry_each() {
        // Codex truncates an `additionalContext` value over 4,000 bytes; the adapter splits longer
        // text at line breaks, so a section that fits is never cut.
        for s in [DELEGATION, SUBAGENT, ORCHESTRATOR] {
            assert!(s.len() < 4_000, "{} bytes", s.len());
        }
    }
}
