import { describe, expect, it } from "vitest";
import { TEMPLATES } from "./automation-templates";
import { automationSpecD, STEP_CONFIGS } from "./automations-api";
import { decode } from "./decode";
import { linearTemplates } from "./linear-templates";

/** The Linear agent as the server installs it (`linear/templates.rs`, `agent_json`). */
const LINEAR_AGENT = JSON.parse(
  String.raw`{"name":"Linear agent","description":"Assign or mention roll on a Linear issue: an agent works on it in a worktree, its progress and questions show in Linear, and its change is linked to the issue.","trigger":{"type":"integration","source":"linear","events":["agent_session.created","agent_session.prompted"]},"defaults":{"target":{"type":"any_online"},"project":{"type":"repo","repo":"{{event.data.repo}}"},"worktree":true},"steps":[{"id":"ack","kind":"linear.activity","on_error":"continue","config":{"type":"thought","body":"On it: roll is getting an agent onto {{event.data.issue.identifier}}."}},{"id":"started","kind":"linear.update_issue","on_error":"continue","when":[{"field":"event.kind","op":"equals","value":"agent_session.created"},{"field":"event.data.issue.state.type","op":"any_of","value":["triage","backlog","unstarted"]}],"config":{"state":"started"}},{"id":"start","kind":"agent.start","when":[{"field":"event.data.thread.thread","op":"exists","not":true}],"config":{"title":"{{event.data.issue.identifier}} {{event.data.issue.title}}","prompt":"You're working on the Linear issue {{event.data.issue.identifier}}: {{event.data.issue.title}}\n{{event.data.issue.url}}\n\n{{event.data.issue.description}}\n\n{{event.data.prompt_context}}\n\nWhat {{event.data.user.name | default:\"they\"}} asked: {{event.data.prompt | default:\"work on this issue\"}}\n\nWork in this repository. If something is unclear, ask: the question goes to Linear and the answer comes back. When you're done, sum up what you changed in a few lines; your last message is posted on the issue."}},{"id":"session","kind":"linear.agent_session","config":{}},{"id":"publish","kind":"agent.publish","when":[{"field":"steps.session.outcome","op":"equals","value":"done"}],"config":{"thread":"{{steps.session.thread}}","message":"{{event.data.issue.identifier}}: {{event.data.issue.title}}"}},{"id":"link","kind":"linear.attach","on_error":"continue","when":[{"field":"steps.publish.url","op":"exists"}],"config":{"url":"{{steps.publish.url}}","title":"roll change {{steps.publish.change}}","subtitle":"{{steps.publish.branch}}"}}]}`,
) as unknown;

/** Drop what serde fills in when it's missing (null, empty lists and objects, the defaults), to compare with what's written. */
function written(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(written);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, x] of Object.entries(v)) {
      if (x === null || x === undefined || (Array.isArray(x) && x.length === 0)) continue;
      if ((k === "enabled" && x === true) || (k === "concurrency" && x === "parallel")) continue;
      out[k] = written(x);
    }
    return out;
  }
  return v;
}

const by = (id: string) => linearTemplates.find((x) => x.id === id)!.spec;

describe("Linear templates", () => {
  it("have unique ids, and each spec decodes strictly to itself", () => {
    expect(new Set(linearTemplates.map((t) => t.id)).size).toBe(linearTemplates.length);
    expect(linearTemplates.map((t) => t.id)).toEqual(["linear-agent", "linear-answer-comment", "linear-steer-comments", "linear-label-agent", "linear-bug-triage"]);
    for (const t of linearTemplates) {
      const json = JSON.parse(JSON.stringify(t.spec)) as unknown;
      expect(decode(automationSpecD, json, { strict: true })).toEqual(t.spec);
    }
  });

  it("are in the gallery as Linear's, each building its own copy", () => {
    for (const t of linearTemplates) {
      const g = TEMPLATES.find((x) => x.id === t.id)!;
      expect(g.integration).toBe("linear");
      expect(g.trigger).toBe("linear");
      expect(g.steps).toEqual(t.spec.steps.map((s) => s.kind));
      const built = g.build({ machines: [], projects: [] });
      expect(built).toEqual(t.spec);
      expect(built).not.toBe(t.spec);
    }
    expect(new Set(TEMPLATES.map((t) => t.id)).size).toBe(TEMPLATES.length);
  });

  it("use only step ids the engine takes, and built-in steps' configs decode strictly as their kind's", () => {
    for (const t of linearTemplates) {
      for (const step of t.spec.steps) {
        expect(step.id).toMatch(/^[a-z0-9_]{1,32}$/);
        const d = STEP_CONFIGS[step.kind as keyof typeof STEP_CONFIGS];
        if (d) expect(written(decode(d, step.config, { strict: true }))).toEqual(written(step.config));
        else expect(step.kind).toMatch(/^linear\./);
      }
    }
  });

  it("the Linear agent is the one the server installs", () => {
    expect(written(by("linear-agent"))).toEqual(LINEAR_AGENT);
    expect(by("linear-agent")).toEqual(decode(automationSpecD, LINEAR_AGENT, { strict: true }));
  });

  it("listen to the events they're about", () => {
    expect(by("linear-answer-comment").trigger).toEqual({ type: "integration", source: "linear", events: ["comment.created"], config: null });
    expect(by("linear-answer-comment").steps.map((s) => [s.id, s.kind, (s.config as { otherwise: string }).otherwise])).toEqual([
      ["approve", "agent.respond", "accept"],
      ["decline", "agent.respond", "decline"],
    ]);
    expect(by("linear-steer-comments").trigger).toEqual({ type: "integration", source: "linear", events: ["comment.created"], config: null });
    expect(by("linear-steer-comments").steps.map((s) => [s.kind, s.config])).toEqual([
      ["agent.message", { thread: "{{event.data.thread.thread}}", text: "{{event.data.comment.user.name}} on {{event.data.issue.identifier}}: {{event.data.comment.body}}", mode: "steer" }],
    ]);
    expect(by("linear-label-agent").trigger).toEqual({ type: "integration", source: "linear", events: ["issue.updated"], config: { label_added: ["roll"] } });
    expect(by("linear-label-agent").steps.map((s) => s.kind)).toEqual(["linear.comment", "agent.start", "agent.wait", "agent.publish", "linear.attach", "linear.comment"]);
    expect(by("linear-bug-triage").trigger).toEqual({ type: "integration", source: "linear", events: ["issue.created"], config: { labels: ["Bug"] } });
    expect(by("linear-bug-triage").steps.map((s) => s.kind)).toEqual(["agent.start", "agent.wait", "linear.comment"]);
    expect((by("linear-bug-triage").steps[0].config as { interaction_mode: string }).interaction_mode).toBe("plan");
  });
});
