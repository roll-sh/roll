import { beforeEach, describe, expect, it } from "vitest";
import { modelKey, modelPrefs, parseModelKey, searchModels, sortModels, type PickerModel } from "./agents-models";

const m = (provider: string, model: string, name: string): PickerModel => ({ key: modelKey(provider, model), provider, providerName: provider === "claude" ? "Claude Code" : "Codex", model, name });
const opus = m("claude", "claude-opus-5-5", "Opus 5.5");
const sonnet = m("claude", "claude-sonnet-5", "Sonnet 5");
const gpt = m("codex", "gpt-6-codex", "GPT-6 Codex");
const mini = m("codex", "gpt-6-mini", "GPT-6 mini");
const all = [opus, sonnet, gpt, mini];

beforeEach(() => modelPrefs.reset());

describe("model keys", () => {
  it("round-trip; model ids may hold colons", () => {
    expect(parseModelKey(modelKey("acp", "openrouter:qwen"))).toEqual({ provider: "acp", model: "openrouter:qwen" });
    expect(parseModelKey("nope")).toBeNull();
    expect(parseModelKey(":x")).toBeNull();
  });
});

describe("ordering (T3's sortProviderModelItems)", () => {
  it("puts favourites first and keeps the catalog order otherwise", () => {
    expect(sortModels([opus, sonnet, gpt], new Set([gpt.key]))).toEqual([gpt, opus, sonnet]);
    expect(sortModels([opus, sonnet], new Set())).toEqual([opus, sonnet]);
  });
});

describe("search (T3's scoreModelPickerSearch)", () => {
  it("ranks an exact name first, then prefixes, word starts and substrings", () => {
    expect(searchModels(all, "sonnet 5", new Set())).toEqual([sonnet]);
    expect(searchModels(all, "gpt", new Set()).map((x) => x.name)).toEqual(["GPT-6 Codex", "GPT-6 mini"]);
    expect(searchModels(all, "mini", new Set())).toEqual([mini]);
    // Provider names match too.
    expect(searchModels(all, "claude", new Set())).toEqual([opus, sonnet]);
  });

});

