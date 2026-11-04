/**
 * Ready-made automations for the Automations view's gallery (the empty state and "Templates").
 * A template builds a whole `AutomationSpec` from what the owner has (machines, projects), so it
 * opens in the builder already pointing at real things. Integrations (Linear, Slack, GitHub) add
 * theirs to `TEMPLATES` with `integration` set: the gallery shows them with the integration's name
 * and the builder says when it needs connecting first.
 */

import type { AutomationSpec, Step, Trigger } from "./automations-api";
import { localZone } from "./automations-ui";

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
          step("tell", "notify", { title: "{{ automation.name }}: {{ steps.wait.status }}", body: "{{ steps.wait.summary | truncate:200 }}", level: "info", link: null }),
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
          step("tell", "notify", { title: "Triage is done", body: "{{ steps.wait.summary | truncate:280 }}", level: "success", link: null }),
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
          step("publish", "agent.publish", { thread: "{{ event.data.thread }}", message: null, land: true }),
          step("settle", "agent.settle", { thread: "{{ event.data.thread }}", action: "settle" }),
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
          thread: "{{ event.data.thread }}",
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
    id: "tell-on-failure",
    title: "Tell me when an agent fails",
    description: "A notice the moment any thread fails, with its error and a link straight to it.",
    trigger: "agent",
    steps: ["notify"],
    build: () =>
      spec("Failures", "A notice for every failed thread.", { type: "agent", events: ["thread.failed"] }, [
        step("tell", "notify", { title: "{{ event.data.title }} failed", body: "{{ event.data.error | truncate:200 }}", level: "error", link: "#agents/{{ event.data.thread }}" }),
      ]),
  },
];
