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

  it("cancels, retries and keeps secret names only", async () => {
    demo = new DemoOwner(0.01, false);
    const api = demo.services().automations!;
    const r = await api.run("demo", "aut_demo_triage", {});
    expect((await api.cancel("demo", r.id)).status).toBe("cancelled");
    expect((await api.retry("demo", r.id)).status).toBe("running");
    await api.setSecret("demo", "aut_demo_triage", "TOKEN", "s3cret");
    const a = await api.get("demo", "aut_demo_triage");
    expect(a.secrets).toEqual(["TOKEN"]);
    expect(JSON.stringify(a)).not.toContain("s3cret");
  });
});
