import { describe, expect, it } from "vitest";
import type { ModelInfo } from "./agents-api";
import { copyFrom, customOption, draftOf, modelFromDraft, presetsFor, type CustomModelDraft } from "./custom-models";

const draft = (over: Partial<CustomModelDraft> = {}): CustomModelDraft => ({ slug: "claude-opus-5-5-preview", name: "", options: [], ...over });

describe("custom models' options (T3's CustomModelEditor logic)", () => {
  it("presets are roll's ids for each provider, the reasoning levels always effort", () => {
    expect(presetsFor("claude").map((p) => p.id)).toEqual(["effort", "fastMode", "thinking"]);
    expect(presetsFor("codex").map((p) => p.id)).toEqual(["effort", "serviceTier"]);
    expect(presetsFor("acp:gemini").map((p) => p.id)).toEqual(["effort"]);
    const effort = modelFromDraft(draft({ options: [presetsFor("claude")[0].make()] })).options![0];
    expect(effort).toEqual({
      type: "select",
      id: "effort",
      label: "Reasoning",
      choices: [
        { id: "low", label: "Low" },
        { id: "medium", label: "Medium" },
        { id: "high", label: "High" },
        { id: "xhigh", label: "Extra High" },
        { id: "max", label: "Max" },
      ],
      default: "high",
      role: "reasoning",
    });
    expect(modelFromDraft(draft({ options: [presetsFor("claude")[1].make()] })).options![0]).toEqual({ type: "boolean", id: "fastMode", label: "Fast Mode", default: false, role: "speed" });
  });

  it("saves trimmed, a choice's empty label as its value, no options as the provider's own, and round-trips", () => {
    const option = { ...customOption(), id: " agent ", label: " Agent ", choices: [{ key: "b", id: "build", label: "" }, { key: "p", id: "plan", label: "Plan" }], defaultChoice: "p" };
    const saved = modelFromDraft(draft({ name: "  Preview  ", options: [option] }));
    expect(saved).toEqual({
      id: "claude-opus-5-5-preview",
      name: "Preview",
      options: [{ type: "select", id: "agent", label: "Agent", choices: [{ id: "build", label: "build" }, { id: "plan", label: "Plan" }], default: "plan" }],
    });
    expect(modelFromDraft(draft())).toEqual({ id: "claude-opus-5-5-preview" });
    expect(modelFromDraft(draftOf(saved))).toEqual(saved);
  });

  it("Copy from… takes a model's options, without Claude's context window", () => {
    const opus: ModelInfo = {
      id: "claude-opus-4-6",
      name: "Claude Opus 4.6",
      default: false,
      options: [
        { type: "boolean", id: "fastMode", label: "Fast Mode", default: false, role: "speed" },
        { type: "select", id: "contextWindow", label: "Context Window", choices: [{ id: "200k", label: "200k" }], default: "200k", role: "context_window" },
      ],
    };
    expect(copyFrom(opus).map((o) => o.id)).toEqual(["fastMode"]);
  });
});
