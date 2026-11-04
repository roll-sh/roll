import { describe, expect, it, vi } from "vitest";
import { DemoAutomations, demoAutomationSpecs, demoRuns } from "./automations-demo";
import { STEP_CONFIGS, automationD, automationRunD } from "./automations-api";
import { decode } from "./decode";

describe("the automations demo", () => {
  it("has believable automations and runs that decode as the relay's", async () => {
    const d = new DemoAutomations();
    const list = await d.list();
    expect(list.length).toBeGreaterThanOrEqual(4);
    for (const a of list) {
      decode(automationD, JSON.parse(JSON.stringify(a)), { strict: true });
      for (const s of a.steps) {
        const dec = STEP_CONFIGS[s.kind as keyof typeof STEP_CONFIGS];
        if (dec) decode(dec, JSON.parse(JSON.stringify(s.config)), { strict: true });
      }
    }
    for (const r of demoRuns(Date.now())) decode(automationRunD, JSON.parse(JSON.stringify(r)), { strict: true });
    expect(new Set(list.map((a) => a.trigger.type))).toEqual(new Set(["code", "schedule", "manual"]));
    expect(demoAutomationSpecs().find((a) => a.id === "aut_demo_release")?.allow_agents).toBe(true);
  });

  it("runs move through their steps and announce it", async () => {
    vi.useFakeTimers();
    const seen: string[] = [];
    const d = new DemoAutomations((ev) => {
      if (ev.type === "automation_run") seen.push(`${ev.run.status}:${ev.run.steps.filter((s) => s.status === "succeeded").length}`);
    }, 10);
    const r = await d.run("acme", "aut_demo_release", { input: { version: "1.4.0" } });
    expect(r.status).toBe("running");
    await vi.runAllTimersAsync();
    const done = await d.runDetail("acme", r.id);
    expect(done.run.status).toBe("succeeded");
    expect(done.run.threads).toHaveLength(1);
    expect(seen.at(-1)).toBe("succeeded:4");
    expect((await d.runs("acme", { automation: "aut_demo_release" }))[0].id).toBe(r.id);
    vi.useRealTimers();
  });

  it("cancels, retries and keeps secret names only", async () => {
    const d = new DemoAutomations(() => {}, 1);
    const r = await d.run("acme", "aut_demo_triage");
    expect((await d.cancel("acme", r.id)).status).toBe("cancelled");
    const retried = await d.retry("acme", r.id);
    expect(retried.status).toBe("running");
    await d.setSecret("acme", "aut_demo_triage", "TOKEN");
    expect((await d.get("acme", "aut_demo_triage")).secrets).toEqual(["TOKEN"]);
    d.stop();
  });
});
