/**
 * Linear's automations for the gallery (`TEMPLATES` in `automation-templates.ts` lists them with
 * `integration: "linear"`). The specs are written as the server reads them (serde's defaults left
 * out) and decoded strictly here, so each is a complete `AutomationSpec`. "Linear agent" is the
 * one the server installs when Linear is connected as roll's app
 * (`crates/code-control/src/agents/linear/templates.rs`); the two must stay the same.
 */
import { automationSpecD, catalogD, type AutomationSpec, type Catalog } from "./automations-api";
import { decode } from "./decode";
import linearCatalog from "./linear-catalog.json";

/**
 * Linear's trigger and step kinds as the server's catalog has them (`linear-catalog.json`, written
 * by `code-control/tests/linear.rs`): for the demo, and for checking the templates.
 */
export const LINEAR_CATALOG: Catalog = decode(catalogD, linearCatalog);

export interface LinearTemplate {
  id: string;
  title: string;
  description: string;
  spec: AutomationSpec;
}

/** The Linear agent's first message to the agent (the server's `templates::PROMPT`). */
export const LINEAR_AGENT_PROMPT =
  "You're working on the Linear issue {{event.data.issue.identifier}}: {{event.data.issue.title}}\n" +
  "{{event.data.issue.url}}\n\n" +
  "{{event.data.issue.description}}\n\n" +
  "{{event.data.prompt_context}}\n\n" +
  'What {{event.data.user.name | default:"they"}} asked: {{event.data.prompt | default:"work on this issue"}}\n\n' +
  "Work in this repository. If something is unclear, ask: the question goes to Linear and the answer comes back. " +
  "When you're done, sum up what you changed in a few lines; your last message is posted on the issue.";

const spec = (raw: unknown): AutomationSpec => decode(automationSpecD, raw, { strict: true });

/** The team's repo, on any machine that has it, in a worktree. */
const inTheTeamsRepo = { target: { type: "any_online" }, project: { type: "repo", repo: "{{event.data.repo}}" }, worktree: true };

/** The change, linked on the issue (and the session, when there is one). */
const linkTheChange = {
  id: "link",
  kind: "linear.attach",
  on_error: "continue",
  when: [{ field: "steps.publish.url", op: "exists" }],
  config: { url: "{{steps.publish.url}}", title: "roll change {{steps.publish.change}}", subtitle: "{{steps.publish.branch}}" },
};

const linearAgent = spec({
  name: "Linear agent",
  description:
    "Assign or mention roll on a Linear issue: an agent works on it in a worktree, its progress and questions show in Linear, and its change is linked to the issue.",
  trigger: { type: "integration", source: "linear", events: ["agent_session.created", "agent_session.prompted"] },
  defaults: inTheTeamsRepo,
  steps: [
    {
      id: "ack",
      kind: "linear.activity",
      on_error: "continue",
      config: { type: "thought", body: "On it: roll is getting an agent onto {{event.data.issue.identifier}}." },
    },
    {
      id: "started",
      kind: "linear.update_issue",
      on_error: "continue",
      when: [
        { field: "event.kind", op: "equals", value: "agent_session.created" },
        { field: "event.data.issue.state.type", op: "any_of", value: ["triage", "backlog", "unstarted"] },
      ],
      config: { state: "started" },
    },
    {
      id: "start",
      kind: "agent.start",
      when: [{ field: "event.data.thread.thread", op: "exists", not: true }],
      config: { title: "{{event.data.issue.identifier}} {{event.data.issue.title}}", prompt: LINEAR_AGENT_PROMPT },
    },
    { id: "session", kind: "linear.agent_session", config: {} },
    {
      id: "publish",
      kind: "agent.publish",
      when: [{ field: "steps.session.outcome", op: "equals", value: "done" }],
      config: { thread: "{{steps.session.thread}}", message: "{{event.data.issue.identifier}}: {{event.data.issue.title}}" },
    },
    linkTheChange,
  ],
});

/** A comment on an issue whose thread goes on, that isn't in a session (those are its prompts). */
const onTheIssuesThread = [
  { field: "event.data.thread.thread", op: "exists" },
  { field: "event.data.in_session", op: "equals", value: true, not: true },
];

const answerFromComment = spec({
  name: "Answer roll from a Linear comment",
  description:
    "A comment saying approve or decline on an issue whose agent waits on you answers its request; a decline passes the comment on as the reason.",
  trigger: { type: "integration", source: "linear", events: ["comment.created"] },
  filters: onTheIssuesThread,
  steps: [
    {
      id: "approve",
      kind: "agent.respond",
      when: [{ field: "event.data.comment.body", op: "contains", value: "approve" }],
      config: { thread: "{{event.data.thread.thread}}", otherwise: "accept" },
    },
    {
      id: "decline",
      kind: "agent.respond",
      when: [{ field: "event.data.comment.body", op: "contains", value: "decline" }],
      config: { thread: "{{event.data.thread.thread}}", otherwise: "decline", message: "{{event.data.comment.body}}" },
    },
  ],
});

const steerFromComments = spec({
  name: "Steer from Linear comments",
  description: "A comment on an issue an agent is working on goes to that agent, as a steer. Comments in its session and mentions of roll are left to the Linear agent.",
  trigger: { type: "integration", source: "linear", events: ["comment.created"] },
  filters: [...onTheIssuesThread, { field: "event.data.mentions_roll", op: "equals", value: true, not: true }],
  steps: [
    {
      id: "steer",
      kind: "agent.message",
      config: {
        thread: "{{event.data.thread.thread}}",
        text: "{{event.data.comment.user.name}} on {{event.data.issue.identifier}}: {{event.data.comment.body}}",
        mode: "steer",
      },
    },
  ],
});

const ISSUE = "{{event.data.issue.identifier}}: {{event.data.issue.title}}\n{{event.data.issue.url}}\n\n{{event.data.issue.description}}";

const labelAgent = spec({
  name: "Label an issue for roll",
  description: "Add the roll label to a Linear issue: an agent works on it in the team's repo, publishes a change, links it to the issue and sums up what it did.",
  trigger: { type: "integration", source: "linear", events: ["issue.updated"], config: { label_added: ["roll"] } },
  defaults: inTheTeamsRepo,
  steps: [
    { id: "ack", kind: "linear.comment", on_error: "continue", config: { body: "roll is on it." } },
    {
      id: "start",
      kind: "agent.start",
      config: {
        title: "{{event.data.issue.identifier}} {{event.data.issue.title}}",
        prompt: `You're working on the Linear issue ${ISSUE}\n\nWork in this repository. When you're done, sum up what you changed in a few lines.`,
      },
    },
    { id: "wait", kind: "agent.wait", config: { thread: "{{steps.start.thread}}" } },
    {
      id: "publish",
      kind: "agent.publish",
      when: [{ field: "steps.wait.outcome", op: "equals", value: "done" }],
      config: { thread: "{{steps.start.thread}}", message: "{{event.data.issue.identifier}}: {{event.data.issue.title}}" },
    },
    linkTheChange,
    {
      id: "summary",
      kind: "linear.comment",
      when: [{ field: "steps.wait.outcome", op: "equals", value: "done" }],
      config: { body: "roll's done: {{steps.publish.url}}\n\n{{steps.wait.message}}" },
    },
  ],
});

const bugTriage = spec({
  name: "Triage new bugs",
  description: "A new Linear issue labelled Bug: an agent in plan mode looks into it and comments with the likely cause and a proposed fix, without changing any code.",
  trigger: { type: "integration", source: "linear", events: ["issue.created"], config: { labels: ["Bug"] } },
  defaults: inTheTeamsRepo,
  steps: [
    {
      id: "start",
      kind: "agent.start",
      config: {
        title: "Triage {{event.data.issue.identifier}}",
        interaction_mode: "plan",
        prompt:
          `A bug was reported in Linear: ${ISSUE}\n\n` +
          "Investigate it in this repository without changing any code: find where it most likely comes from, " +
          "say how sure you are, and propose a fix (files and the change). Keep it to a short comment.",
      },
    },
    { id: "wait", kind: "agent.wait", config: { thread: "{{steps.start.thread}}" } },
    {
      id: "comment",
      kind: "linear.comment",
      when: [{ field: "steps.wait.outcome", op: "equals", value: "done" }],
      config: { body: "roll looked into this:\n\n{{steps.wait.message}}" },
    },
  ],
});

export const linearTemplates: LinearTemplate[] = [
  {
    id: "linear-agent",
    title: "Linear issue assigned to roll: an agent in a worktree, its change linked back",
    description: "Assign or mention roll on an issue: an agent works on it, its progress and questions show in Linear, and its change is linked.",
    spec: linearAgent,
  },
  {
    id: "linear-answer-comment",
    title: "Answer a roll request from a Linear comment",
    description: "Comment approve or decline on the issue, and the agent's waiting request is answered.",
    spec: answerFromComment,
  },
  {
    id: "linear-steer-comments",
    title: "Steer from Linear comments",
    description: "New comments on an issue reach the agent working on it.",
    spec: steerFromComments,
  },
  {
    id: "linear-label-agent",
    title: "Label an issue for roll",
    description: "The roll label starts an agent on the issue; it links its change and sums up. Works with an API key too.",
    spec: labelAgent,
  },
  {
    id: "linear-bug-triage",
    title: "Triage new bugs",
    description: "New Bug issues get an agent's read on the cause and a proposed fix, as a comment.",
    spec: bugTriage,
  },
];
