import { describe, expect, it } from "vitest";
import { decode } from "./decode";
import { providerInfoD, type ModelInfo, type ModelOption } from "./agents-api";
import { acceptedOptions, effortValue, modelEfforts, nextEffort, reasoningLevels } from "./agents-model-options";

const fast: ModelOption = { type: "boolean", id: "fastMode", label: "Fast Mode", default: false, role: "speed" };
const tiers: ModelOption = {
  type: "select",
  id: "serviceTier",
  label: "Service Tier",
  choices: [
    { id: "default", label: "Standard" },
    { id: "priority", label: "Fast" },
    { id: "ultrafast", label: "Ultrafast" },
  ],
  default: "default",
  role: "speed",
};
const ctx: ModelOption = {
  type: "select",
  id: "contextWindow",
  label: "Context Window",
  choices: [
    { id: "200k", label: "200k" },
    { id: "1m", label: "1M" },
  ],
  default: "1m",
  role: "context_window",
};
const opus: ModelInfo = { id: "claude-opus-4-6", name: "Claude Opus 4.6", default: true, options: [reasoningLevels(["low", "high", "max"], "high"), fast, ctx] };
const gpt: ModelInfo = { id: "gpt", name: "GPT", default: true, options: [reasoningLevels(["low", "xhigh"]), tiers] };

describe("model options (T3's traits)", () => {
  it("a model switch keeps the options the new model takes; the effort is never an option", () => {
    expect(acceptedOptions(gpt, { fastMode: true, contextWindow: "1m", serviceTier: "priority", effort: "low" })).toEqual({ serviceTier: "priority" });
    expect(acceptedOptions(opus, { fastMode: true, contextWindow: "2m" })).toEqual({ fastMode: true });
    expect(acceptedOptions(undefined, { fastMode: true })).toEqual({});
  });

  it("efforts, the one in effect, and stepping through them", () => {
    expect(modelEfforts(opus)).toEqual(["low", "high", "max"]);
    expect(effortValue(opus, null)).toBe("high");
    expect(effortValue(gpt, null)).toBeUndefined();
    expect(nextEffort(opus, null)).toBe("max");
    expect(nextEffort(opus, "max")).toBe("low");
    expect(nextEffort(opus, "low", -1)).toBe("max");
    // Nothing picked and no default: the first step lands on the first level.
    expect(nextEffort(gpt, null)).toBe("low");
    expect(nextEffort({ ...gpt, options: [] }, null)).toBeNull();
  });
});

describe("provider info from a daemon before option descriptors", () => {
  it("reads `efforts` as the reasoning option, and `images` defaults to on", () => {
    const info = decode(providerInfoD, {
      id: "codex",
      name: "Codex",
      installed: true,
      auth: "ready",
      models: [{ id: "gpt-5.5", name: "GPT-5.5", default: true, efforts: ["minimal", "xhigh"] }],
      capabilities: { steer: true, fork: true, rollback: true, plan_mode: true, images: true, subagents: true, mode_switch: true },
    });
    expect(info.update_required).toEqual([]);
    const m = info.models[0];
    expect(m.images).toBeUndefined();
    expect(m.options).toEqual([reasoningLevels(["minimal", "xhigh"])]);
    expect(m.options[0].type === "select" && m.options[0].choices.map((c) => c.label)).toEqual(["Minimal", "Extra High"]);
  });
});
