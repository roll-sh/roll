/**
 * Ready-made automations for the Automations view's gallery (the empty state and "Templates").
 * A template builds a whole `AutomationSpec` from what the owner has (machines, projects), so it
 * opens in the builder already pointing at real things. Integrations (Linear, Slack, GitHub) add
 * theirs to `TEMPLATES` with `integration` set: the gallery shows them with the integration's name
 * and the builder says when it needs connecting first.
 */

import type { AutomationSpec, Step, Trigger } from "./automations-api";
import { localZone } from "./automations-ui";
import { linearTemplates } from "./linear-templates";

/** What a template can point at. */
export interface TemplateContext {
  machines: readonly { id: string; name: string; online: boolean; sandbox: boolean }[];
  projects: readonly { id: string; name: string; machine: string; repo: string | null }[];
}

export interface AutomationTemplate {
  id: string;
  title: string;
  description: string;
  /** The trigger's source (`manual`, `schedule`, `code`, `agent`, `webhook`, `linear`, …): the card's icon. */
  trigger: string;
  /** The step kinds it runs, in order: the card's chain of icons. */
  steps: string[];
  /** Set for an integration's template (`linear`, `slack`, …). */
  integration?: string;
  build(ctx: TemplateContext): AutomationSpec;
}

const step = (id: string, kind: string, config: Record<string, unknown>, over: Partial<Step> = {}): Step => ({ id, kind, config, when: [], retry: null, timeout_s: null, ...over });

const spec = (name: string, description: string, trigger: Trigger, steps: Step[], over: Partial<AutomationSpec> = {}): AutomationSpec => ({
  name,
  description,
  enabled: true,
  trigger,
  filters: [],
  steps,
  concurrency: "parallel",
  rate_limit: null,
  run_as: null,
  ...over,
});

/** The machine a template should use: an online computer of the owner's, else any. */
function pickMachine(ctx: TemplateContext) {
  return ctx.machines.find((m) => m.online && !m.sandbox) ?? ctx.machines.find((m) => m.online) ?? ctx.machines[0];
}

function pickProject(ctx: TemplateContext, machine?: string) {
  return ctx.projects.find((p) => p.machine === machine && p.repo) ?? ctx.projects.find((p) => p.machine === machine) ?? ctx.projects[0];
}

const agentStart = (over: Record<string, unknown>) => ({
  target: null,
  project: null,
  provider: null,
  model: null,
  effort: null,
  runtime_mode: "full_access",
  interaction_mode: null,
  worktree: true,
  title: null,
  prompt: "",
  ...over,
});

// ── chat ops (docs/code/chatops.md): Slack, Discord and GitHub driving the agents ──

/** agent.start's output, where the chat status steps want it. */
const started = { machine: "{{ steps.start.machine }}", thread: "{{ steps.start.thread }}" };
/** Run a step only when the agent finished one way or the other (`agent.wait`'s outcome). */
const after = (outcome: "done" | "failed") => ({ when: [{ field: "steps.wait.outcome", op: "equals" as const, value: outcome }] });
/** The owner's machine to run on, as agent.start's target. */
const onMachine = (ctx: TemplateContext) => {
  const m = pickMachine(ctx);
  const p = pickProject(ctx, m?.id);
  return { target: m ? { type: "machine", machine: m.id } : { type: "any_online" }, project: p ? { type: "id", id: p.id } : null };
};
const inSandbox = { target: { type: "sandbox", size: "standard" }, project: { type: "repo", repo: "{{ event.data.repo }}", path: null } };

/** Chat ops' templates (their `integration` says what to connect first). */
export const CHATOPS_TEMPLATES: AutomationTemplate[] = [
  {
    id: "slack-roll-command",
    title: "/roll in Slack: an agent on your machine",
    description: "/roll <prompt> or a mention of roll starts an agent on your machine, with a live status in the Slack thread: its approvals and questions are buttons, and replies there steer it.",
    trigger: "slack",
    steps: ["agent.start", "slack.status"],
    integration: "slack",
    build: (ctx) =>
      spec("/roll in Slack", "An agent from /roll or a mention, with a live status in the thread.", { type: "integration", source: "slack", events: ["command", "mention"], config: {} }, [
        step("start", "agent.start", agentStart({ ...onMachine(ctx), title: "{{ event.data.prompt | truncate:60 }}", prompt: "{{ event.data.prompt }}" })),
        step("status", "slack.status", { ...started }),
      ]),
  },
  {
    id: "github-mention-pr",
    title: "@roll on a GitHub issue: a sandbox, then a PR",
    description: "@roll in an issue comment starts an agent in a new sandbox on the repo. When it's done, its work is opened as a pull request and linked from the issue.",
    trigger: "github",
    steps: ["agent.start", "agent.wait", "agent.publish", "github.pr", "github.comment", "github.comment"],
    integration: "github",
    build: () =>
      spec(
        "@roll on GitHub issues",
        "Work on an issue in a sandbox when someone mentions @roll, then open a pull request.",
        { type: "integration", source: "github", events: ["comment.mention"], config: {} },
        [
          step(
            "start",
            "agent.start",
            agentStart({
              ...inSandbox,
              title: "#{{ event.data.number }} {{ event.data.title }}",
              prompt: "{{ event.data.prompt }}\n\nFrom @{{ event.data.author }} on issue #{{ event.data.number }} of {{ event.data.repo }}: {{ event.data.title }}",
            }),
          ),
          step("wait", "agent.wait", { thread: "{{ steps.start.thread }}", until: ["done", "failed"], timeout_s: 7200 }),
          step("publish", "agent.publish", { thread: "{{ steps.start.thread }}", message: "{{ event.data.title }} (#{{ event.data.number }})" }, after("done")),
          step("pr", "github.pr", { change: "{{ steps.publish.change }}", body: "Closes #{{ event.data.number }}.\n\n{{ steps.wait.message }}" }, after("done")),
          step("comment", "github.comment", { body: "Opened {{ steps.pr.url }} for this." }, after("done")),
          step("failed", "github.comment", { body: 'roll couldn\'t finish this: {{ steps.wait.error | default:"the agent stopped" }}' }, after("failed")),
        ],
        // Pull requests have their change already; this one is for issues.
        { filters: [{ field: "event.data.is_pr", op: "equals", value: false }] },
      ),
  },
  {
    id: "check-failed-fix",
    title: "A check fails on main: an agent fixes it",
    description: "When a GitHub check fails on main, an agent in a sandbox fixes the cause and queues the fix to land; a commit status says how it went.",
    trigger: "github",
    steps: ["agent.start", "agent.wait", "agent.publish", "github.status", "github.status"],
    integration: "github",
    build: () =>
      spec(
        "Fix failed checks on main",
        "An agent fixes a check that failed on main and queues the fix to land.",
        { type: "integration", source: "github", events: ["check.failed"], config: { branches: ["main"] } },
        [
          step(
            "start",
            "agent.start",
            agentStart({
              ...inSandbox,
              title: "Fix {{ event.data.check }} on {{ event.data.branch }}",
              prompt: "The check {{ event.data.check }} failed on {{ event.data.branch }} at {{ event.data.sha }}.\nIts output: {{ event.data.url }}\n\nFind out why, fix the cause rather than the test, and run it again until it passes.",
            }),
          ),
          step("wait", "agent.wait", { thread: "{{ steps.start.thread }}", until: ["done", "failed"], timeout_s: 7200 }),
          step("publish", "agent.publish", { thread: "{{ steps.start.thread }}", message: "Fix {{ event.data.check }} on {{ event.data.branch }}", land: true }, after("done")),
          step("status", "github.status", { state: "pending", context: "roll/fix", description: "A fix is queued to land", target_url: "{{ steps.publish.url }}" }, after("done")),
          step("gave_up", "github.status", { state: "failure", context: "roll/fix", description: "roll's agent couldn't fix it" }, after("failed")),
        ],
        // One fix at a time: a second failure waits for the first fix.
        { concurrency: "queue" },
      ),
  },
  {
    id: "slack-agent-needs-me",
    title: "Tell me in Slack when an agent needs me",
    description: "A Slack DM when an agent waits on you, fails or finishes, with buttons for its approvals and questions; a reply steers it.",
    trigger: "agent",
    steps: ["slack.notify"],
    integration: "slack",
    build: () =>
      spec("Agents in my Slack DMs", "A DM when an agent waits on me, fails or is done.", { type: "agent", events: ["thread.waiting", "thread.failed", "thread.done"] }, [
        // The machine and thread come from the agent event; `to` is your Slack member id, which only
        // you know: left empty, so the builder asks for it before it's created (a made-up id DMs nobody).
        step("notify", "slack.notify", { to: "" }),
      ]),
  },
  {
    id: "slack-reaction-handoff",
    title: "🤖 on a Slack message hands it to roll",
    description: "React with :robot_face: to a Slack message and an agent on your machine takes it as its brief, with a live status in the message's thread.",
    trigger: "slack",
    steps: ["agent.start", "slack.status"],
    integration: "slack",
    build: (ctx) =>
      spec("Hand a Slack message to roll", "A :robot_face: reaction starts an agent on the message.", { type: "integration", source: "slack", events: ["reaction"], config: { reactions: ["robot_face"] } }, [
        step("start", "agent.start", agentStart({ ...onMachine(ctx), title: "{{ event.data.text | truncate:60 }}", prompt: "{{ event.data.text }}" })),
        step("status", "slack.status", { ...started, channel: "{{ event.data.channel }}", thread_ts: "{{ event.data.thread_ts }}" }),
      ]),
  },
];

export const TEMPLATES: AutomationTemplate[] = [
  {
    id: "run-on-machine",
    title: "Run: an agent in a worktree",
    description: "When you press Run, an agent starts on your machine in its own worktree with the prompt you give it.",
    trigger: "manual",
    steps: ["agent.start", "agent.wait", "notify"],
    build(ctx) {
      const m = pickMachine(ctx);
      const p = pickProject(ctx, m?.id);
      return spec(
        m ? `Run on ${m.name}` : "Run an agent",
        "An agent in a fresh worktree, started by hand.",
        { type: "manual" },
        [
          step(
            "start",
            "agent.start",
            agentStart({
              target: m ? { type: "machine", machine: m.id } : { type: "any_online" },
              project: p ? { type: "id", id: p.id } : null,
              prompt: '{{ event.data.input.prompt | default:"Look at the open work items and pick one to fix." }}',
            }),
          ),
          step("wait", "agent.wait", { thread: "{{ steps.start.thread }}", until: ["done", "failed", "waiting"], timeout_s: 7200, on_timeout: "continue" }),
          step("tell", "notify", { title: "{{ automation.name }}: {{ steps.wait.outcome }}", body: "{{ steps.wait.message | truncate:200 }}", level: "info", link: null }),
        ],
      );
    },
  },
  {
    id: "weekday-triage",
    title: "Weekdays 9:00: triage new work",
    description: "Every weekday morning an agent reads the new work items, labels and prioritises them, and leaves a summary.",
    trigger: "schedule",
    steps: ["agent.start", "agent.wait", "notify"],
    build(ctx) {
      const m = pickMachine(ctx);
      const p = pickProject(ctx, m?.id);
      return spec(
        "Morning triage",
        "Labels and prioritises yesterday's new work items.",
        { type: "schedule", cron: "0 9 * * 1-5", timezone: localZone() },
        [
          step(
            "triage",
            "agent.start",
            agentStart({
              target: { type: "project_machine" },
              project: p ? { type: "id", id: p.id } : null,
              runtime_mode: "auto_edit",
              worktree: false,
              title: "Triage",
              prompt:
                "Read the work items opened since the last triage. For each: add labels, set a priority, and close duplicates with a link to the original. Finish with a short summary of what needs a person.",
            }),
          ),
          step("wait", "agent.wait", { thread: "{{ steps.triage.thread }}", until: ["done", "failed"], timeout_s: 3600, on_timeout: "continue" }),
          step("tell", "notify", { title: "Triage is done", body: "{{ steps.wait.message | truncate:280 }}", level: "success", link: null }),
        ],
        { concurrency: "skip" },
      );
    },
  },
  {
    id: "main-breaks",
    title: "When main breaks: an agent fixes it",
    description: "A broken-main work item starts an agent in a sandbox that finds the cause, fixes it and queues the fix to land.",
    trigger: "code",
    steps: ["agent.start", "agent.wait", "agent.publish"],
    build(ctx) {
      const p = ctx.projects.find((x) => x.repo) ?? ctx.projects[0];
      return spec(
        "Fix a broken main",
        "Main is red: find the cause, fix it, queue the fix.",
        { type: "code", events: ["work"], repos: p?.repo ? [p.repo] : [] },
        [
          step(
            "fix",
            "agent.start",
            agentStart({
              target: { type: "sandbox", size: "standard" },
              project: p?.repo ? { type: "repo", repo: p.repo, path: null } : p ? { type: "id", id: p.id } : null,
              title: "Fix main: {{ event.data.title }}",
              prompt: "Main is broken: {{ event.data.title }}.\n\n{{ event.data.body | truncate:4000 }}\n\nFind the commit that broke it, fix the cause (not the symptom), and make the checks pass.",
            }),
            { retry: { attempts: 2, backoff_s: 60 } },
          ),
          step("wait", "agent.wait", { thread: "{{ steps.fix.thread }}", until: ["done", "failed"], timeout_s: 5400, on_timeout: "fail" }),
          step("land", "agent.publish", { thread: "{{ steps.fix.thread }}", message: "Fix main: {{ event.data.title }}", land: true }, { when: [{ field: "steps.wait.status", op: "equals", value: "done" }] }),
        ],
        {
          filters: [
            { field: "event.data.labels", op: "has_label", value: "broken-main" },
            { field: "event.data.state", op: "equals", value: "open" },
          ],
          concurrency: "cancel_previous",
        },
      );
    },
  },
  {
    id: "publish-when-done",
    title: "When a thread finishes: publish and land",
    description: "Every agent that finishes its work in a worktree publishes it as a change and queues it to land.",
    trigger: "agent",
    steps: ["agent.publish", "agent.settle"],
    build: () =>
      spec(
        "Publish finished work",
        "Done threads become changes in the merge queue.",
        { type: "agent", events: ["thread.done"] },
        [
          step("publish", "agent.publish", { thread: "{{ event.data.thread.id }}", land: true }),
          step("settle", "agent.settle", { thread: "{{ event.data.thread.id }}", action: "settle" }),
        ],
      ),
  },
  {
    id: "answer-safe-commands",
    title: "Approve tests and builds",
    description: "When an agent asks to run tests, a build or a linter, say yes; everything else still waits for you.",
    trigger: "agent",
    steps: ["agent.respond"],
    build: () =>
      spec("Approve tests and builds", "Yes to test, build and lint commands.", { type: "agent", events: ["request.opened"] }, [
        step("answer", "agent.respond", {
          thread: "{{ event.data.thread.id }}",
          rules: [
            { action: "command", command: "cargo test*", files: null, tool: null, decision: "accept" },
            { action: "command", command: "pnpm test*", files: null, tool: null, decision: "accept" },
            { action: "command", command: "pnpm lint*", files: null, tool: null, decision: "accept" },
          ],
          otherwise: null,
          message: null,
        }),
      ]),
  },
  {
    id: "webhook-agent",
    title: "Webhook: an agent from anywhere",
    description: "A signed POST from any service (CI, an alert, a form) starts an agent with the request's body as its brief.",
    trigger: "webhook",
    steps: ["agent.start"],
    build(ctx) {
      const p = pickProject(ctx, pickMachine(ctx)?.id);
      return spec("Agent from a webhook", "POST a brief, get an agent.", { type: "webhook" }, [
        step(
          "start",
          "agent.start",
          agentStart({
            target: { type: "any_online" },
            project: p ? { type: "id", id: p.id } : null,
            title: '{{ event.data.body.title | default:"From a webhook" }}',
            prompt: "{{ event.data.body.prompt }}",
          }),
        ),
      ]);
    },
  },
  {
    id: "visual-review-waits",
    title: "Tell me when a visual change waits for a review",
    description: "A notice when a change's preview looks different from main and nobody has accepted it yet (roll previews' visual review), with a link to its screenshots.",
    trigger: "code",
    steps: ["notify"],
    build: (ctx) => {
      const p = ctx.projects.find((x) => x.repo) ?? ctx.projects[0];
      return spec(
        "Visual changes to review",
        "A notice for every change whose pages differ from main's.",
        { type: "code", events: ["preview"], repos: p?.repo ? [p.repo] : [] },
        [
          step("tell", "notify", {
            title: "A visual change waits for a review",
            body: "Its pages differ from {{ event.data.preview.environment.visual_review.branch }} by {{ event.data.preview.environment.visual_review.percent }}%.",
            level: "info",
            link: "{{ event.data.preview.environment.dashboard_url }}",
          }),
        ],
        { filters: [{ field: "event.data.preview.environment.visual_review.state", op: "equals", value: "changed" }] },
      );
    },
  },
  {
    id: "tell-on-failure",
    title: "Tell me when an agent fails",
    description: "A notice the moment any thread fails, with its error and a link straight to it.",
    trigger: "agent",
    steps: ["notify"],
    build: () =>
      spec("Failures", "A notice for every failed thread.", { type: "agent", events: ["thread.failed"] }, [
        step("tell", "notify", { title: "{{ event.data.thread.title }} failed", body: "{{ event.data.run.error | truncate:200 }}", level: "error", link: "#agents/{{ event.data.thread.id }}" }),
      ]),
  },
  ...CHATOPS_TEMPLATES,
  // Linear's (`linear-templates.ts`): they work in the team's repo (the Linear card's settings) on
  // any machine that has it, so they don't need the owner's machines picked.
  ...linearTemplates.map(
    (t): AutomationTemplate => ({
      id: t.id,
      title: t.title,
      description: t.description,
      trigger: "linear",
      steps: t.spec.steps.map((s) => s.kind),
      integration: "linear",
      build: () => structuredClone(t.spec),
    }),
  ),
];
