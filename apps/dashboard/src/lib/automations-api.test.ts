import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { describe, expect, it, vi } from "vitest";
import {
  STEP_CONFIGS,
  STEP_KINDS,
  automationD,
  automationSavedD,
  catalogD,
  createAutomationsApi,
  noticeD,
  runDetailD,
  runNowD,
  testRequestD,
  testResultD,
  type Automation,
  type Trigger,
} from "./automations-api";
import { decode, type Decoder } from "./decode";

const DIR = join(__dirname, "..", "..", "..", "..", "crates", "agent-proto", "fixtures");

/** Every automation fixture file and the decoder for its items. A new file fails until it's listed here. */
const decoders: Record<string, Decoder<unknown>> = {
  "automations.json": automationD,
  "automation_saved.json": automationSavedD,
  "automation_runs.json": runDetailD,
  "automation_notices.json": noticeD,
  "automation_catalogs.json": catalogD,
  "automation_tests.json": testRequestD,
  "automation_test_results.json": testResultD,
  "automation_run_now.json": runNowD,
};

const load = (file: string): unknown[] => JSON.parse(readFileSync(join(DIR, file), "utf8"));

/** Drop what Rust skips (null, empty lists), so a decoded record compares with what Rust wrote. */
function prune(v: unknown): unknown {
  if (Array.isArray(v)) return v.map(prune);
  if (v && typeof v === "object") {
    const out: Record<string, unknown> = {};
    for (const [k, x] of Object.entries(v)) {
      if (x === null || x === undefined || (Array.isArray(x) && x.length === 0)) continue;
      out[k] = prune(x);
    }
    return out;
  }
  return v;
}

describe("automation fixtures", () => {
  it("every automation fixture file has a decoder", () => {
    const files = readdirSync(DIR).filter((f) => f.startsWith("automation") && f.endsWith(".json"));
    expect(files.sort()).toEqual(Object.keys(decoders).sort());
  });

  for (const [file, d] of Object.entries(decoders)) {
    it(`${file} decodes strictly and keeps every field`, () => {
      for (const item of load(file)) {
        expect(prune(decode(d, item, { strict: true }))).toEqual(prune(item));
      }
    });
  }

  it("every built-in step's config decodes strictly as its kind's", () => {
    const automations = load("automations.json").map((a) => decode(automationD, a, { strict: true }));
    const kinds = new Set<string>();
    for (const step of automations.flatMap((a) => a.steps)) {
      const d = STEP_CONFIGS[step.kind as keyof typeof STEP_CONFIGS];
      if (!d) continue;
      kinds.add(step.kind);
      expect(prune(decode(d, step.config, { strict: true }))).toEqual(prune(step.config));
    }
    expect([...kinds].sort()).toEqual([...STEP_KINDS].sort());
  });

  it("cover every trigger", () => {
    const types = new Set(load("automations.json").map((a) => (a as Automation).trigger.type));
    const all: Record<Trigger["type"], true> = {
      manual: true,
      schedule: true,
      interval: true,
      webhook: true,
      code: true,
      agent: true,
      integration: true,
    };
    expect([...types].sort()).toEqual(Object.keys(all).sort());
  });
});

describe("automations client", () => {
  it("calls the routes and decodes their answers", async () => {
    const [automation] = load("automations.json");
    const [detail] = load("automation_runs.json") as { run: unknown }[];
    const call = vi.fn(async (method: string, path: string, _body?: unknown): Promise<unknown> => {
      if (path.endsWith("/runs/arn_1")) return detail;
      if (path.includes("/runs") || (method === "POST" && path.endsWith("/run"))) return method === "GET" ? [detail.run] : detail.run;
      if (method === "GET" && path.endsWith("/automations")) return [automation];
      if (method === "DELETE" || (method === "PUT" && path.includes("/secrets/"))) return null;
      return { automation, webhook_secret: null };
    });
    const api = createAutomationsApi(call);
    expect((await api.list("acme"))[0].id).toBe("aut_0000000000000001");
    expect((await api.runDetail("acme", "arn_1")).run.id).toBe("arn_0a0b0c0d0e0f1011");
    expect((await api.run("acme", "aut_x", { key: "k" })).status).toBe("queued");
    expect(call).toHaveBeenLastCalledWith("POST", "/v1/code/agents/acme/automations/aut_x/run", { input: null, key: "k" });
    await api.runs("acme", { automation: "aut_x", limit: 20 });
    expect(call).toHaveBeenLastCalledWith("GET", "/v1/code/agents/acme/automations/runs?automation=aut_x&limit=20");
    await api.setSecret("acme", "aut_x", "TOKEN", "v");
    expect(call).toHaveBeenLastCalledWith("PUT", "/v1/code/agents/acme/automations/aut_x/secrets/TOKEN", { value: "v" });
    const saved = await api.update("acme", "aut_x", decode(automationD, automation), 3);
    expect(saved.automation.revision).toBe(1);
    expect((call.mock.lastCall?.[2] as { revision: number }).revision).toBe(3);
  });
});
