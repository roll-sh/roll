import { describe, expect, it } from "vitest";
import { CHATOPS_TEMPLATES, TEMPLATES, type TemplateContext } from "./automation-templates";
import { automationSpecD, STEP_CONFIGS, STEP_KINDS, type BuiltInStepKind } from "./automations-api";
import { decode } from "./decode";

/**
 * Chat ops' step kinds and their configs' keys (`additionalProperties: false`), as registered in
 * crates/code-control/src/chatops/{slack,discord,github}.rs.
 */
const INTEGRATION_STEPS: Record<string, { keys: string[]; required: string[] }> = {
  "slack.post": { keys: ["team", "channel", "text", "blocks", "thread_ts"], required: ["channel", "text"] },
  "slack.update": { keys: ["team", "channel", "ts", "text", "blocks"], required: ["channel", "ts", "text"] },
  "slack.reply": { keys: ["team", "channel", "thread_ts", "text", "blocks"], required: ["text"] },
  "slack.dm": { keys: ["team", "user", "text", "blocks"], required: ["user", "text"] },
  "slack.status": { keys: ["team", "machine", "thread", "channel", "thread_ts", "ts"], required: ["machine", "thread"] },
  "slack.notify": { keys: ["team", "to", "machine", "thread", "text"], required: ["to"] },
  "discord.post": { keys: ["guild", "channel", "user", "text"], required: ["text"] },
  "discord.status": { keys: ["guild", "machine", "thread", "channel", "message"], required: ["machine", "thread"] },
  "discord.notify": { keys: ["guild", "channel", "user", "machine", "thread"], required: [] },
  "github.comment": { keys: ["repo", "number", "body"], required: ["body"] },
  "github.label": { keys: ["repo", "number", "add", "remove"], required: [] },
  "github.status": { keys: ["repo", "sha", "state", "context", "description", "target_url"], required: ["state"] },
  "github.pr": { keys: ["repo", "change", "title", "body", "draft"], required: ["change"] },
};

/** Integration trigger sources: their events and config keys. */
const SOURCES: Record<string, { events: string[]; config: string[] }> = {
  slack: { events: ["command", "mention", "message", "reaction"], config: ["team", "channels", "reactions"] },
  discord: { events: ["command"], config: ["guild", "channels"] },
  github: { events: ["issue.opened", "issue.labeled", "comment.mention", "review.requested", "check.failed"], config: ["repos", "labels", "branches", "anyone"] },
};

const isBuiltIn = (k: string): k is BuiltInStepKind => (STEP_KINDS as readonly string[]).includes(k);

const ctx: TemplateContext = {
  machines: [
    { id: "mch_sand", name: "sandbox-1", online: true, sandbox: true },
    { id: "mch_box", name: "build-box", online: true, sandbox: false },
  ],
  projects: [{ id: "prj_app", name: "app", machine: "mch_box", repo: "acme/app" }],
};

describe("chat ops' automation templates", () => {
  it("are in the gallery, once each", () => {
    const ids = TEMPLATES.map((t) => t.id);
    expect(new Set(ids).size).toBe(ids.length);
    expect(CHATOPS_TEMPLATES.map((t) => t.id)).toEqual(["slack-roll-command", "github-mention-pr", "check-failed-fix", "slack-agent-needs-me", "slack-reaction-handoff"]);
    for (const t of CHATOPS_TEMPLATES) expect(TEMPLATES).toContain(t);
  });

  it.each(CHATOPS_TEMPLATES.map((t) => [t.id, t] as const))("%s builds a spec that decodes strictly, from known kinds and sources", (_id, t) => {
    const built = t.build(ctx);
    const spec = decode(automationSpecD, JSON.parse(JSON.stringify(built)) as unknown, { strict: true });
    expect(spec.steps.map((s) => s.kind)).toEqual(t.steps);
    const used = new Set<string>();
    const { trigger, steps } = spec;
    if (trigger.type === "integration") {
      const src = SOURCES[trigger.source];
      expect(src, `trigger source ${trigger.source}`).toBeDefined();
      for (const e of trigger.events) expect(src.events).toContain(e);
      for (const k of Object.keys(trigger.config as object)) expect(src.config).toContain(k);
      used.add(trigger.source);
    }
    const ids = new Set<string>();
    for (const s of steps) {
      expect(s.id).toMatch(/^[a-z0-9_]{1,32}$/);
      expect(ids.has(s.id), `step id ${s.id} twice`).toBe(false);
      ids.add(s.id);
      // Steps only read the outputs of steps before them.
      for (const ref of JSON.stringify(s.config).matchAll(/steps\.([a-z0-9_]+)\./g)) expect(ids.has(ref[1]) && ref[1] !== s.id, `${s.id} reads ${ref[1]}`).toBe(true);
      if (isBuiltIn(s.kind)) {
        decode(STEP_CONFIGS[s.kind], s.config, { strict: true });
        continue;
      }
      const known = INTEGRATION_STEPS[s.kind];
      expect(known, `step kind ${s.kind}`).toBeDefined();
      const config = s.config as Record<string, unknown>;
      for (const k of Object.keys(config)) expect(known.keys, `${s.kind}.${k}`).toContain(k);
      for (const k of known.required) expect(config[k], `${s.kind} needs ${k}`).toBeTruthy();
      used.add(s.kind.split(".")[0]);
    }
    expect([...used]).toEqual([t.integration]);
  });

  it("run on the owner's computer and hand agent.start's output to the chat steps", () => {
    const slack = CHATOPS_TEMPLATES.find((t) => t.id === "slack-roll-command")!.build(ctx);
    expect(slack.steps[0].config).toMatchObject({ target: { type: "machine", machine: "mch_box" }, project: { type: "id", id: "prj_app" }, prompt: "{{ event.data.prompt }}" });
    expect(slack.steps[1].config).toEqual({ machine: "{{ steps.start.machine }}", thread: "{{ steps.start.thread }}" });
    // No machines yet: any online one.
    const bare = CHATOPS_TEMPLATES.find((t) => t.id === "slack-roll-command")!.build({ machines: [], projects: [] });
    expect(bare.steps[0].config).toMatchObject({ target: { type: "any_online" }, project: null });

    const gh = CHATOPS_TEMPLATES.find((t) => t.id === "github-mention-pr")!.build(ctx);
    expect(gh.filters).toEqual([{ field: "event.data.is_pr", op: "equals", value: false }]);
    expect(gh.steps.find((s) => s.kind === "github.pr")!.config).toMatchObject({ change: "{{ steps.publish.change }}" });
    const fix = CHATOPS_TEMPLATES.find((t) => t.id === "check-failed-fix")!.build(ctx);
    expect(fix.trigger).toMatchObject({ source: "github", events: ["check.failed"], config: { branches: ["main"] } });
    expect(fix.concurrency).toBe("queue");
    expect(fix.steps.find((s) => s.kind === "agent.publish")!.config).toMatchObject({ land: true });
  });
});
