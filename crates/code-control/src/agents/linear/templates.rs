//! The built-in Linear automations. Connecting roll as Linear's agent (OAuth) installs
//! [`agent`] for the owner once ("Linear agent": assigning or mentioning roll on an issue starts
//! an agent on it); it's an ordinary automation the owner edits or turns off. The dashboard's
//! template gallery (`apps/dashboard/src/lib/linear-templates.ts`) has the same one and more.

use super::SOURCE;
use crate::Code;
use crate::agents::automations::registry::listens;
use agent_proto::automation::AutomationSpec;
use serde_json::{Value, json};

/// The agent's first message, from the issue and what was asked.
pub const PROMPT: &str = "You're working on the Linear issue {{event.data.issue.identifier}}: {{event.data.issue.title}}\n\
{{event.data.issue.url}}\n\n\
{{event.data.issue.description}}\n\n\
{{event.data.prompt_context}}\n\n\
What {{event.data.user.name | default:\"they\"}} asked: {{event.data.prompt | default:\"work on this issue\"}}\n\n\
Work in this repository. If something is unclear, ask: the question goes to Linear and the answer comes back. \
When you're done, sum up what you changed in a few lines; your last message is posted on the issue.";

/// "Linear agent": an agent session (roll assigned or mentioned, or a reply that no run is
/// taking) → say so at once, move the issue to started, start an agent in a worktree of the
/// team's repo on any machine that has it (unless the issue has a thread already: then that one
/// goes on), show its work in the session, publish the change and link it.
pub fn agent_json() -> Value {
    json!({
        "name": "Linear agent",
        "description": "Assign or mention roll on a Linear issue: an agent works on it in a worktree, its progress and questions show in Linear, and its change is linked to the issue.",
        "trigger": { "type": "integration", "source": SOURCE, "events": ["agent_session.created", "agent_session.prompted"] },
        "defaults": { "target": { "type": "any_online" }, "project": { "type": "repo", "repo": "{{event.data.repo}}" }, "worktree": true },
        "steps": [
            { "id": "ack", "kind": "linear.activity", "on_error": "continue",
              "config": { "type": "thought", "body": "On it: roll is getting an agent onto {{event.data.issue.identifier}}." } },
            { "id": "started", "kind": "linear.update_issue", "on_error": "continue",
              "when": [
                  { "field": "event.kind", "op": "equals", "value": "agent_session.created" },
                  { "field": "event.data.issue.state.type", "op": "any_of", "value": ["triage", "backlog", "unstarted"] }
              ],
              "config": { "state": "started" } },
            { "id": "start", "kind": "agent.start",
              "when": [{ "field": "event.data.thread.thread", "op": "exists", "not": true }],
              "config": { "title": "{{event.data.issue.identifier}} {{event.data.issue.title}}", "prompt": PROMPT } },
            { "id": "session", "kind": "linear.agent_session", "config": {} },
            { "id": "publish", "kind": "agent.publish",
              "when": [{ "field": "steps.session.outcome", "op": "equals", "value": "done" }],
              "config": { "thread": "{{steps.session.thread}}", "message": "{{event.data.issue.identifier}}: {{event.data.issue.title}}" } },
            { "id": "link", "kind": "linear.attach", "on_error": "continue",
              "when": [{ "field": "steps.publish.url", "op": "exists" }],
              "config": { "url": "{{steps.publish.url}}", "title": "roll change {{steps.publish.change}}", "subtitle": "{{steps.publish.branch}}" } }
        ]
    })
}

pub fn agent() -> AutomationSpec {
    serde_json::from_value(agent_json()).expect("the Linear agent template parses")
}

/// Who the installed automation is from (its `created_by`).
pub const BY: &str = "linear";

/// Install the default automations for a newly connected owner: the Linear agent, once ever (not
/// again after the owner turned it off or deleted it, nor when one of theirs already takes agent
/// sessions). Best effort: connecting goes on. `by` is who connected.
pub async fn install_defaults(code: &std::sync::Arc<Code>, owner_id: &str, by: &str) {
    let has = code.db.sql().read(|c| {
        let ours: bool = c.query_row("SELECT EXISTS (SELECT 1 FROM automations WHERE owner_id = ? AND created_by = ?)", code_sql::params![owner_id, BY], |r| r.get(0))?;
        Ok::<_, code_sql::Error>(ours || listens(c, owner_id, SOURCE, "agent_session.created")?)
    });
    match has {
        Ok(true) => {}
        Ok(false) => match code.create_automation(owner_id, agent(), BY).await {
            Ok(saved) => tracing::info!(automation = %saved.automation.id, connected_by = by, "installed the Linear agent automation"),
            Err(e) => tracing::warn!(error = %e, "installing the Linear agent automation"),
        },
        Err(e) => tracing::warn!(error = %e, "looking for the owner's Linear automations"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_linear_agent_template_parses_and_uses_linear_and_agent_steps() {
        let s = agent();
        assert_eq!(s.trigger.source(), SOURCE);
        let kinds: Vec<&str> = s.steps.iter().map(|st| st.kind.as_str()).collect();
        assert_eq!(kinds, ["linear.activity", "linear.update_issue", "agent.start", "linear.agent_session", "agent.publish", "linear.attach"]);
        // Every string in the configs is a template that parses.
        fn strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
            match v {
                Value::String(s) => out.push(s),
                Value::Array(a) => a.iter().for_each(|v| strings(v, out)),
                Value::Object(o) => o.values().for_each(|v| strings(v, out)),
                _ => {}
            }
        }
        for st in &s.steps {
            let mut all = vec![];
            strings(&st.config, &mut all);
            for t in all {
                assert!(crate::agents::automations::template::Template::parse(t).is_ok(), "{}: {t}", st.id);
            }
        }
    }
}
