import { describe, expect, it } from "vitest";
import type { AutomationSpec, Catalog } from "./automations-api";
import { TEMPLATES } from "./automation-templates";
import { STEP_KINDS } from "./automations-api";
import { CHATOPS_CATALOG, withChatops } from "./chatops-catalog";
import { LINEAR_CATALOG } from "./linear-templates";
import { blankSpec, cronShape, cronText, defaultConfig, ev, insertVariable, matchVariables, newStep, newStepId, openTemplate, sampleEvent, schemaOf, validate, variablesAt, withBuiltIns, withoutUnset } from "./automations-ui";
import { hashFor, viewFromHash, type AgentsView } from "./agents-surface";

const catalog = withBuiltIns(null);

const step = (id: string, kind: string, config: Record<string, unknown> = {}) => ({ id, kind, config, when: [], retry: null, timeout_s: null });

describe("automations: templates in fields", () => {
  it("finds the open {{ under the caret, and puts a variable in its place", () => {
    const text = "Fix {{ event.da";
    const at = openTemplate(text, text.length)!;
    expect(at).toEqual({ start: 4, query: "event.da" });
    expect(insertVariable(text, at.start, text.length, "event.data.title")).toEqual({ text: "Fix {{ event.data.title }}", caret: 26 });
    // Closed already, or past it: nothing to complete.
    expect(openTemplate("{{ a }} b", 9)).toBeNull();
    // An auto-closed `}}` after the caret is the one it closes.
    expect(insertVariable("{{}} tail", 0, 2, "run.id").text).toBe("{{ run.id }} tail");
  });

  it("offers the trigger's fields, the run, and only earlier steps' outputs", () => {
    const spec: AutomationSpec = { ...blankSpec(), trigger: { type: "agent", events: ["thread.done"] }, steps: [step("start", "agent.start"), step("wait", "agent.wait"), step("tell", "notify")] };
    const at2 = variablesAt(spec, catalog, 2).map((v) => v.path);
    expect(at2).toContain("event.data.thread.id");
    expect(at2).toContain("event.data.thread.title");
    expect(at2).toContain("steps.start.thread");
    expect(at2).toContain("steps.wait.status");
    expect(at2).not.toContain("steps.tell.id");
    const at0 = variablesAt(spec, catalog, 0).map((v) => v.path);
    expect(at0.some((p) => p.startsWith("steps."))).toBe(false);
    // Prefix matches come first, the `event.data.` prefix may be left out.
    expect(matchVariables(variablesAt(spec, catalog, 2), "thr")[0].path).toBe("event.data.thread.id");
  });

});

describe("automations: validation", () => {
  it("says what's wrong, where", () => {
    const spec: AutomationSpec = {
      ...blankSpec(),
      trigger: { type: "schedule", cron: "every day", timezone: "UTC" },
      filters: [{ field: "event.data.repo", op: "equals", value: "" }],
      steps: [step("start", "agent.start", { prompt: "" }), step("start", "notify", { title: "{{ steps.later.x }} {{ secrets.T }}" }), step("later", "delay", { seconds: 5 })],
    };
    const p = validate(spec, catalog);
    expect(p.name).toBeDefined();
    expect(p["trigger.cron"]).toMatch(/5 fields/);
    expect(p["filters.0"]).toBeDefined();
    expect(p["steps.0.config.prompt"]).toMatch(/needed/);
    expect(p["steps.1.id"]).toMatch(/Another step/);
    expect(p["steps.1.config.title"]).toMatch(/Secrets only go in a webhook's headers|runs after/);
    expect(validate({ ...spec, name: "ok", trigger: { type: "manual" }, filters: [], steps: [step("a", "delay", { seconds: 1 })] }, catalog)).toEqual({});
  });

  it("allows secrets in a webhook step's headers only", () => {
    const spec = (headers: Record<string, string>, body: unknown): AutomationSpec => ({ ...blankSpec(), name: "x", steps: [step("hook", "webhook", { url: "https://x.test", headers, body })] });
    expect(validate(spec({ Authorization: "Bearer {{ secrets.TOKEN }}" }, null), catalog)).toEqual({});
    expect(validate(spec({}, { token: "{{ secrets.TOKEN }}" }), catalog)["steps.0.config.body.token"]).toMatch(/Secrets/);
  });

  it("gives new steps ids nobody has", () => {
    expect(newStepId("agent.start", [])).toBe("start");
    expect(newStepId("agent.start", ["start", "start_2"])).toBe("start_3");
    expect(newStepId("linear.comment", [])).toBe("comment");
  });
});

describe("automations: a new step's config", () => {
  // Every kind the builder offers: the built-ins, chat ops' and Linear's (the server's own schemas).
  const all = withChatops(withBuiltIns({ triggers: [], steps: LINEAR_CATALOG.steps }));
  const kinds = all.steps;

  it.each(kinds.map((s) => [s.kind, s] as const))("%s starts with only its defaults and what it needs", (kind, info) => {
    const schema = schemaOf(info);
    const config = newStep(kind, all, []).config as Record<string, unknown>;
    expect(config).toEqual(defaultConfig(schema));
    for (const [k, v] of Object.entries(config)) {
      // An unset field is absent: the server refused `null` for maps, lists and flags.
      expect(v, `${kind}.${k}`).not.toBeNull();
      expect(schema.properties?.[k]?.default !== undefined || schema.required?.includes(k), `${kind}.${k} is neither a default nor required`).toBe(true);
    }
  });

  it("covers every kind there is", () => {
    expect(kinds.length).toBe(STEP_KINDS.length + CHATOPS_CATALOG.steps.length + LINEAR_CATALOG.steps.length);
  });

  it("drops what a form clears", () => {
    expect(withoutUnset({ a: 1, b: null, c: undefined, d: "", e: false, f: [] })).toEqual({ a: 1, d: "", e: false, f: [] });
  });
});

describe("automations: the catalog and samples", () => {
  it("fills in the built-ins around the server's catalog, keeping its integrations", () => {
    const server: Catalog = {
      triggers: [{ source: "linear", title: "Linear", description: "", events: [ev("issue.created", "Issue created", ["issue.title"])], config: null, needs_setup: true }],
      steps: [{ kind: "notify", title: "Notify", description: "", config: { required: ["title"], type: "object" }, output: ["id"], integration: null }],
    };
    const c = withBuiltIns(server);
    expect(c.triggers.map((t) => t.source)).toEqual(["manual", "schedule", "interval", "webhook", "code", "agent", "linear"]);
    // A built-in step without a form gets the dashboard's.
    expect((c.steps.find((s) => s.kind === "notify")!.config as { properties: object }).properties).toHaveProperty("title");
    expect(c.steps.map((s) => s.kind)).toContain("agent.start");
  });

  it("makes a sample event from the trigger's fields, or takes the server's", () => {
    // The server's sample of a work item (automation-catalog.json), in the trigger's repo.
    const e = sampleEvent({ type: "code", events: ["work"], repos: ["acme/web"] }, catalog, 5);
    expect(e).toMatchObject({ source: "code", kind: "work", data: { repo: "acme/web", number: 7, title: "main is broken: test", labels: ["broken-main"] } });
    // No sample: made from the fields.
    const bare = withBuiltIns({ triggers: [{ source: "acme", title: "", description: "", events: [ev("x", "X", ["number", "labels"])], config: null }], steps: [] });
    expect(sampleEvent({ type: "integration", source: "acme", events: ["x"], config: {} }, bare, 5).data).toEqual({ number: 42, labels: ["bug"] });
    const withSample = withBuiltIns({ triggers: [{ source: "manual", title: "", description: "", events: [ev("run", "Run", [], { by: "ada" })], config: null }], steps: [] });
    expect(sampleEvent({ type: "manual" }, withSample).data).toEqual({ by: "ada" });
  });

  it("builds every template into a spec that validates", () => {
    const ctx = { machines: [{ id: "mch_1", name: "mbp", online: true, sandbox: false }], projects: [{ id: "prj_1", name: "app", machine: "mch_1", repo: "acme/app" }] };
    // Integrations' templates against the catalog with their entries (as a server that has them;
    // Linear's as its registry writes them).
    const withIntegrations = withChatops(withBuiltIns(LINEAR_CATALOG));
    for (const t of TEMPLATES) {
      const spec = t.build(ctx);
      // Only what the owner alone knows is left to fill (the Slack DM template's member id).
      const left = t.id === "slack-agent-needs-me" ? { "steps.0.config.to": expect.any(String) } : {};
      expect(validate(spec, t.integration ? withIntegrations : catalog), t.id).toEqual(left);
      expect(spec.steps.map((s) => s.kind), t.id).toEqual(t.steps);
    }
  });
});

describe("automations: links", () => {
  it("round-trips every automations view through the hash", () => {
    const views: AgentsView[] = [
      { kind: "automations" },
      { kind: "automations", list: "runs" },
      { kind: "automations", list: "templates" },
      { kind: "automations", open: "new" },
      { kind: "automations", open: "new", template: "main-breaks" },
      { kind: "automations", open: "aut_1" },
      { kind: "automations", open: "aut_1", tab: "settings" },
      { kind: "automations", open: "aut_1", tab: "runs" },
      { kind: "automations", open: "aut_1", tab: "runs", run: "arn_9" },
    ];
    for (const v of views) expect(viewFromHash(hashFor(v))).toEqual(v);
    expect(hashFor({ kind: "automations", open: "aut_1", tab: "runs", run: "arn_9" })).toBe("#agents/automations/aut_1/runs/arn_9");
    // Threads keep their links.
    expect(viewFromHash("#agents/thr_1")).toEqual({ kind: "thread", thread: "thr_1" });
  });
});

describe("cron patterns", () => {
  it("refuse a value outside its field, as the server does, and say which", () => {
    expect(cronShape("0 9 * * 1-5")).toBeNull();
    expect(cronShape("*/15 0-23 1,15 1-12 0-7")).toBeNull();
    expect(cronShape("30 0 9 * * MON#2")).toBeNull();
    expect(cronShape("0 25 * * *")).toBe("The hour is 0 to 23: 25 isn't one.");
    expect(cronShape("60 * * * *")).toBe("The minute is 0 to 59: 60 isn't one.");
    expect(cronShape("0 0 0 * *")).toBe("The day of the month is 1 to 31: 0 isn't one.");
    expect(cronShape("0 0 1 13 *")).toBe("The month is 1 to 12: 13 isn't one.");
    expect(cronShape("61 0 9 * * *")).toBe("The second is 0 to 59: 61 isn't one.");
  });

  it("take @daily and the other names, say how many fields there were, and refuse a date that never comes (as the server does)", () => {
    for (const m of ["@daily", "@hourly", "@weekly", "@monthly", "@yearly", "@annually", "@midnight", "@DAILY"]) expect(cronShape(m)).toBeNull();
    expect(cronShape("@sometimes")).toBe("Use @hourly, @daily, @weekly, @monthly or @yearly, or 5 fields.");
    expect(cronText("@daily")).toBe("Daily at 00:00");
    expect(cronText("@weekly")).toBe("Sundays at 00:00");
    expect(cronText("@monthly")).toBe("Monthly on day 1 at 00:00");
    expect(cronText("@yearly")).toBe("Yearly on 1 January at 00:00");
    expect(cronShape("0 9 * *")).toBe("A cron expression has 5 fields (minute hour day month weekday), or 6 with seconds first: this has 4.");
    expect(cronShape("0 0 9 * * * *")).toMatch(/this has 7\.$/);
    // Six fields: seconds first, said as such.
    expect(cronShape("*/10 * * * * *")).toBeNull();
    expect(cronText("*/10 * * * * *")).toBe("Every 10 seconds");
    expect(cronText("0 0 9 * * 1-5")).toBe("Weekdays at 09:00");
    expect(cronText("30 0 9 * * *")).toBeNull();
    expect(cronShape("0 9 31 2 *")).toBe("It would never run: February has no day 31.");
    expect(cronShape("0 9 31 4,6 *")).toBe("It would never run: April or June have no day 31.");
    expect(cronShape("0 9 29 2 *")).toBeNull();
    expect(cronShape("0 9 31 2,3 *")).toBeNull();
    // A weekday too: cron runs on either, so it does run.
    expect(cronShape("0 9 31 2 1")).toBeNull();
  });
});
