import { afterEach, describe, expect, it } from "vitest";
import { DemoOwner } from "./agents-demo";
import { STEP_CONFIGS, automationD, automationRunD, catalogD } from "./automations-api";
import { decode } from "./decode";

describe("the automations demo", () => {
  let demo: DemoOwner;
  afterEach(() => demo?.dispose());

  it("has believable automations, runs and a catalog that decode strictly as the relay's", async () => {
    demo = new DemoOwner(0.01, false);
    const api = demo.services().automations!;
    const list = await api.list("demo");
    expect(list.length).toBeGreaterThanOrEqual(5);
    for (const a of list) {
      decode(automationD, JSON.parse(JSON.stringify(a)), { strict: true });
      for (const s of a.steps) {
        const dec = STEP_CONFIGS[s.kind as keyof typeof STEP_CONFIGS];
        if (dec) decode(dec, JSON.parse(JSON.stringify(s.config)), { strict: true });
      }
    }
    for (const r of await api.runs("demo", { limit: 200 })) decode(automationRunD, JSON.parse(JSON.stringify(r)), { strict: true });
    decode(catalogD, JSON.parse(JSON.stringify(await api.catalog("demo"))), { strict: true });
    expect(new Set(list.map((a) => a.trigger.type))).toEqual(new Set(["code", "schedule", "agent", "manual", "webhook"]));
  });

  it("runs a connected integration's step (they all failed 'connect slack first')", async () => {
    demo = new DemoOwner(0.01, false);
    const api = demo.services().automations!;
    const step = (await api.catalog("demo")).steps.find((k) => k.integration && !k.needs_setup)!;
    expect(step).toBeDefined();
    const saved = await api.create("demo", { name: "Tell the team", enabled: true, trigger: { type: "manual" }, filters: [], steps: [{ id: "tell", kind: step.kind, config: {}, when: [], retry: null, timeout_s: null }], concurrency: "parallel", rate_limit: null, run_as: null });
    const r = await api.run("demo", saved.automation.id, {});
    const t0 = Date.now();
    let now = r;
    while (!["succeeded", "failed"].includes(now.status) && Date.now() - t0 < 8000) {
      await new Promise((res) => setTimeout(res, 50));
      now = (await api.runDetail("demo", r.id)).run;
    }
    expect(now.status).toBe("succeeded");
  });

  it("starts with none in the first-run demo (?demo=empty)", async () => {
    demo = new DemoOwner(0.01, false, true);
    const api = demo.services().automations!;
    expect(await api.list("demo")).toEqual([]);
    expect(await api.runs("demo")).toEqual([]);
    expect(await api.notices("demo")).toEqual([]);
  });

  it("cancels, retries and keeps secret names only", async () => {
    demo = new DemoOwner(0.01, false);
    const api = demo.services().automations!;
    const r = await api.run("demo", "aut_demo_triage", {});
    expect((await api.cancel("demo", r.id)).status).toBe("cancelled");
    const again = await api.retry("demo", r.id);
    expect(again.status).toBe("running");
    // As the server: the run says when, and the stopped step's log says why.
    expect(again.retried_at).toBeGreaterThan(0);
    expect(again.steps.some((s) => s.log.some((l) => l.text === "Retried after the run was cancelled"))).toBe(true);
    await api.setSecret("demo", "aut_demo_triage", "TOKEN", "s3cret");
    const a = await api.get("demo", "aut_demo_triage");
    expect(a.secrets).toEqual(["TOKEN"]);
    expect(JSON.stringify(a)).not.toContain("s3cret");
  });
});
