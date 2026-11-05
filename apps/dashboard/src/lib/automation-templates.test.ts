import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { CHATOPS_TEMPLATES, TEMPLATES, type TemplateContext } from "./automation-templates";
import { automationSpecD, STEP_CONFIGS, STEP_KINDS, type BuiltInStepKind } from "./automations-api";
import { BUILT_IN_STEPS, withBuiltIns, validate } from "./automations-ui";
import { CHATOPS_CATALOG, withChatops } from "./chatops-catalog";
import { decode } from "./decode";

/** Chat ops' trigger sources and step kinds, from the server's registry (chatops-catalog.json). */
const SOURCES = Object.fromEntries(CHATOPS_CATALOG.triggers.map((t) => [t.source, t]));
const STEPS = Object.fromEntries(CHATOPS_CATALOG.steps.map((s) => [s.kind, s.config as { properties: Record<string, unknown>; required?: string[] }]));

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
      for (const e of trigger.events) expect(src.events.map((x) => x.kind)).toContain(e);
      const props = Object.keys((src.config as { properties: object }).properties);
      for (const k of Object.keys(trigger.config as object)) expect(props).toContain(k);
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
      const known = STEPS[s.kind];
      expect(known, `step kind ${s.kind}`).toBeDefined();
      const config = s.config as Record<string, unknown>;
      for (const k of Object.keys(config)) expect(Object.keys(known.properties), `${s.kind}.${k}`).toContain(k);
      for (const k of known.required ?? []) expect(config[k], `${s.kind} needs ${k}`).toBeTruthy();
      used.add(s.kind.split(".")[0]);
    }
    expect([...used]).toEqual([t.integration]);
    // The builder's own checks, on a server that has chat ops.
    expect(validate(built, withChatops(withBuiltIns(null)))).toEqual({});
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

describe("step outputs", () => {
  /** Each built-in step's output, as the server declares it (`info(step_kinds::X, …, &[…])` in steps.rs). */
  const serverOutputs = (): Record<string, string[]> => {
    const root = join(__dirname, "../../../..");
    const kinds = Object.fromEntries([...readFileSync(join(root, "crates/agent-proto/src/automation.rs"), "utf8").matchAll(/pub const ([A-Z_]+): &str = "([^"]+)";/g)].map((m) => [m[1], m[2]]));
    const src = readFileSync(join(root, "crates/code-control/src/agents/automations/steps.rs"), "utf8");
    const out: Record<string, string[]> = {};
    for (const m of src.matchAll(/info\(\s*step_kinds::([A-Z_]+),[\s\S]*?&\[([^\]]*)\],?\s*\)/g)) out[kinds[m[1]]] = [...m[2].matchAll(/"([^"]+)"/g)].map((x) => x[1]);
    return out;
  };

  it("the dashboard's built-in steps say what the server's steps put out", () => {
    const server = serverOutputs();
    expect(Object.keys(server).sort()).toEqual([...STEP_KINDS].sort());
    for (const kind of STEP_KINDS) expect(BUILT_IN_STEPS[kind].output, kind).toEqual(server[kind]);
  });

  it("templates only read outputs their earlier steps have", () => {
    const outputs: Record<string, string[]> = { ...Object.fromEntries(STEP_KINDS.map((k) => [k, BUILT_IN_STEPS[k].output])), ...Object.fromEntries(CHATOPS_CATALOG.steps.map((s) => [s.kind, s.output])) };
    for (const t of TEMPLATES) {
      const spec = t.build(ctx);
      const kindOf = Object.fromEntries(spec.steps.map((s) => [s.id, s.kind]));
      for (const [, id, field] of JSON.stringify(spec).matchAll(/steps\.([a-z_0-9]+)\.([a-z_0-9]+)/g)) {
        const known = outputs[kindOf[id]];
        if (known) expect(known, `${t.id}: steps.${id}.${field}`).toContain(field);
      }
    }
  });
});
