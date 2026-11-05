import { describe, expect, it } from "vitest";
import type { Automation, AutomationRun, AutomationSpec, Catalog } from "./automations-api";
import { TEMPLATES } from "./automation-templates";
import { withChatops } from "./chatops-catalog";
import { LINEAR_CATALOG } from "./linear-templates";
import {
  blankSpec,
  cronShape,
  cronText,
  ev,
  insertVariable,
  lastRunOf,
  matchVariables,
  newStepId,
  openTemplate,
  sampleEvent,
  templateParts,
  triggerSummary,
  validate,
  variablesAt,
  weekOf,
  withBuiltIns,
} from "./automations-ui";
import { hashFor, viewFromHash, type AgentsView } from "./agents-surface";

const catalog = withBuiltIns(null);

const step = (id: string, kind: string, config: Record<string, unknown> = {}) => ({ id, kind, config, when: [], retry: null, timeout_s: null });

describe("automations: words", () => {
  it("says common cron schedules in words, and leaves the rest alone", () => {
    expect(cronText("0 9 * * 1-5")).toBe("Weekdays at 09:00");
    expect(cronText("30 18 * * *")).toBe("Daily at 18:30");
    expect(cronText("0 * * * *")).toBe("Every hour");
    expect(cronText("*/15 * * * *")).toBe("Every 15 minutes");
    expect(cronText("0 8 * * 1")).toBe("Mondays at 08:00");
    expect(cronText("0 0 9 * * 1-5")).toBe("Weekdays at 09:00");
    expect(cronText("0 9 1-7 * 1")).toBeNull();
  });

  it("summarises each trigger in one line", () => {
    expect(triggerSummary({ type: "manual" })).toBe("When you run it");
    expect(triggerSummary({ type: "schedule", cron: "0 9 * * 1-5", timezone: "Europe/Budapest" })).toBe("Weekdays at 09:00 · Budapest");
    expect(triggerSummary({ type: "interval", every_s: 900 })).toBe("Every 15 minutes");
    expect(triggerSummary({ type: "code", events: ["landed", "check"], repos: ["acme/app"] })).toBe("When a change lands or a check reports in acme/app");
    expect(triggerSummary({ type: "agent", events: ["thread.done"] })).toBe("When a thread finishes");
  });
});

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
    expect(at2).toContain("event.data.thread");
    expect(at2).toContain("steps.start.thread");
    expect(at2).toContain("steps.wait.status");
    expect(at2).not.toContain("steps.tell.id");
    const at0 = variablesAt(spec, catalog, 0).map((v) => v.path);
    expect(at0.some((p) => p.startsWith("steps."))).toBe(false);
    // Prefix matches come first, the `event.data.` prefix may be left out.
    expect(matchVariables(variablesAt(spec, catalog, 2), "thr")[0].path).toBe("event.data.thread");
  });

  it("splits text and tokens for the highlighter", () => {
    expect(templateParts("a {{ x }} b")).toEqual([
      { text: "a ", token: false },
      { text: "{{ x }}", token: true },
      { text: " b", token: false },
    ]);
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
    const e = sampleEvent({ type: "code", events: ["work"], repos: ["acme/web"] }, catalog, 5);
    expect(e).toMatchObject({ source: "code", kind: "work", data: { repo: "acme/web", number: 42, labels: ["bug"] } });
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

describe("automations: runs", () => {
  const run = (id: string, at: number, status: AutomationRun["status"], automation = "aut_1"): AutomationRun => ({
    id,
    automation,
    name: "x",
    status,
    event: { id: "e", source: "manual", kind: "run", summary: "", at },
    steps: [],
    threads: [],
    cursor: 1,
    created_at: at,
    started_at: at,
    finished_at: at + 10,
    reason: null,
  });

  it("counts the last seven days per day, failures apart, skips left out", () => {
    const now = new Date(2026, 10, 4, 12).getTime();
    const day = 86_400_000;
    const w = weekOf([run("a", now - 1000, "succeeded"), run("b", now - day, "failed"), run("c", now - day, "skipped"), run("d", now - 9 * day, "succeeded"), run("e", now, "succeeded", "aut_2")], "aut_1", now);
    expect(w.total).toBe(2);
    expect(w.failed).toBe(1);
    expect(w.days[6]).toEqual({ ok: 1, failed: 0 });
    expect(w.days[5]).toEqual({ ok: 0, failed: 1 });
  });

  it("takes the last run from the live runs over the list's", () => {
    const a = { id: "aut_1", last_run: { id: "old", status: "failed", at: 100 } } as Automation;
    expect(lastRunOf(a, [run("new", 200, "running")])).toMatchObject({ id: "new", status: "running" });
    expect(lastRunOf(a, [])).toMatchObject({ id: "old" });
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
});
