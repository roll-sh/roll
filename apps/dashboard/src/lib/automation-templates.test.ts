import { readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { CHATOPS_TEMPLATES, TEMPLATES, type TemplateContext } from "./automation-templates";
import builtInCatalog from "./automation-catalog.json";
import { automationSpecD, STEP_CONFIGS, STEP_KINDS, type AutomationSpec, type BuiltInStepKind, type Catalog } from "./automations-api";
import { BUILT_IN_STEPS, withBuiltIns, validate } from "./automations-ui";
import { CHATOPS_CATALOG, withChatops } from "./chatops-catalog";
import { decode } from "./decode";
import { LINEAR_CATALOG } from "./linear-templates";

/** Chat ops' trigger sources and step kinds, from the server's registry (chatops-catalog.json). */
const SOURCES = Object.fromEntries(CHATOPS_CATALOG.triggers.map((t) => [t.source, t]));
const STEPS = Object.fromEntries(CHATOPS_CATALOG.steps.map((s) => [s.kind, s.config as { properties: Record<string, unknown>; required?: string[] }]));

/** Fields a template can't fill: only the owner knows them (their own Slack member id). */
const OWNER_FILLS: Record<string, string[]> = { "slack.notify": ["to"] };

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
      // Required fields are filled, except what only the owner knows (left for the builder to ask).
      for (const k of known.required ?? []) if (!OWNER_FILLS[s.kind]?.includes(k)) expect(config[k], `${s.kind} needs ${k}`).toBeTruthy();
      used.add(s.kind.split(".")[0]);
    }
    expect([...used]).toEqual([t.integration]);
    // The builder's own checks, on a server that has chat ops: only the owner's own fields are asked for.
    const asked = Object.keys(validate(built, withChatops(withBuiltIns(null))));
    expect(asked).toEqual(steps.flatMap((s, i) => (OWNER_FILLS[s.kind] ?? []).filter((k) => !(s.config as Record<string, unknown>)[k]).map(() => expect.stringContaining(`${i}`))));
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

describe("templates read what their events bring", () => {
  // Every trigger's events with a sample of their `event.data`, from the server's registry: the
  // built-ins (automation-catalog.json), chat ops' and Linear's.
  const triggers = [...(builtInCatalog as unknown as Catalog).triggers, ...CHATOPS_CATALOG.triggers, ...LINEAR_CATALOG.triggers];

  /** The value at `path` (`thread.title`) of `v`; `undefined` where it has none. */
  const at = (v: unknown, path: string) => path.split(".").reduce<unknown>((o, k) => (o !== null && typeof o === "object" ? (o as Record<string, unknown>)[k] : undefined), v);

  /** The events a spec's trigger listens for. */
  function kindsOf(spec: AutomationSpec): string[] {
    const t = spec.trigger;
    const all = () => triggers.find((x) => x.source === (t.type === "integration" ? t.source : t.type))!.events.map((e) => e.kind);
    switch (t.type) {
      case "manual":
        return ["run"];
      case "schedule":
      case "interval":
        return ["tick"];
      case "webhook":
        return ["post"];
      default:
        return t.events.length ? [...t.events] : all();
    }
  }

  /** Each `{{ … }}` in the strings of `v`: (where, path, pipes). */
  function* refs(v: unknown, where: string): Generator<[string, string, string[]]> {
    if (typeof v === "string") {
      for (const m of v.matchAll(/\{\{([^}]*)\}\}/g)) {
        const [path, ...pipes] = m[1].split("|").map((x) => x.trim());
        yield [where, path, pipes];
      }
    } else if (Array.isArray(v)) for (const [i, x] of v.entries()) yield* refs(x, `${where}.${i}`);
    else if (v && typeof v === "object") for (const [k, x] of Object.entries(v)) yield* refs(x, `${where}.${k}`);
  }

  /** What the sender makes up, so no sample has it: a webhook's body, a manual run's input. */
  const FREE_FORM: Record<string, string> = { webhook: "event.data.body.", manual: "event.data.input." };

  it.each(TEMPLATES.map((t) => [t.id, t] as const))("%s", (id, t) => {
    const spec = t.build(ctx);
    const source = spec.trigger.type === "integration" ? spec.trigger.source : spec.trigger.type;
    const trigger = triggers.find((x) => x.source === source);
    expect(trigger, `${id}: trigger ${source}`).toBeDefined();
    /** Fields a filter makes sure of (`exists`): a step may read them even if a sample has none. */
    const sure = (when: AutomationSpec["filters"]) => when.filter((f) => f.op === "exists" && !f.not).map((f) => f.field);
    const reads = spec.steps.flatMap((st, i) => [...refs(st.config, `steps.${i}.config`)].map(([where, path, pipes]) => ({ where, path, pipes, sure: [...sure(spec.filters), ...sure(st.when)] })));
    for (const kind of kindsOf(spec)) {
      const sample = trigger!.events.find((e) => e.kind === kind)?.sample;
      expect(sample, `${id}: ${source} ${kind} has a sample`).toBeTypeOf("object");
      for (const { where, path, pipes, sure } of reads) {
        if (!path.startsWith("event.data.") || pipes.some((p) => p.startsWith("default")) || path.startsWith(FREE_FORM[source] ?? "\0")) continue;
        if (sure.some((f) => path === f || path.startsWith(`${f}.`))) continue;
        const v = at(sample, path.slice("event.data.".length));
        expect(v, `${id} (${kind}): ${where} reads ${path}, which the event doesn't have`).not.toBeUndefined();
        // A thread id, a title, a link: a template renders a scalar (an object comes out as JSON).
        if (v !== null && typeof v === "object" && !pipes.some((p) => /^(json|join|first|last|length)\b/.test(p))) expect.fail(`${id} (${kind}): ${where} renders ${path}, an object`);
      }
      const filters = [...spec.filters.map((f, i) => [`filters.${i}`, f] as const), ...spec.steps.flatMap((s, i) => s.when.map((f, j) => [`steps.${i}.when.${j}`, f] as const))];
      for (const [where, f] of filters) {
        if (!f.field.startsWith("event.data.") || f.op === "exists") continue;
        expect(at(sample, f.field.slice("event.data.".length)), `${id} (${kind}): ${where} filters on ${f.field}, which the event doesn't have`).not.toBeUndefined();
      }
    }
  });
});
