import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ModelOption, ProviderInfo } from "@/lib/agents-api";
import { reasoningLevels } from "@/lib/agents-model-options";
import { modelPrefs } from "@/lib/agents-models";
import { provider } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { ModelPicker, PermissionsMenu, choiceOf, type ModelChoice } from "./model-picker";

const fast: ModelOption = { type: "boolean", id: "fastMode", label: "Fast Mode", default: false, role: "speed" };
const context: ModelOption = {
  type: "select",
  id: "contextWindow",
  label: "Context Window",
  choices: [
    { id: "200k", label: "200k" },
    { id: "1m", label: "1M" },
  ],
  default: "200k",
  role: "context_window",
};
const thinking: ModelOption = { type: "boolean", id: "thinking", label: "Thinking", default: true };
const tier: ModelOption = {
  type: "select",
  id: "serviceTier",
  label: "Service Tier",
  choices: [
    { id: "default", label: "Standard" },
    { id: "priority", label: "Fast", description: "2x speed" },
  ],
  default: "default",
  role: "speed",
};

const claude = provider({
  update_required: [{ id: "claude-haiku-5-5", name: "Claude Haiku 5.5", min_version: "2.1.293" }],
  models: [
    { id: "claude-opus-5-5", name: "Claude Opus 5.5", short_name: "Opus 5.5", default: true, options: [reasoningLevels(["low", "medium", "high", "max"], "medium"), fast] },
    { id: "claude-sonnet-5", name: "Sonnet 5", default: false, options: [] },
    { id: "claude-sonnet-4-6", name: "Sonnet 4.6", default: false, legacy: true, options: [reasoningLevels(["low", "high"], "high"), context, thinking] },
  ],
});
const codex = provider({
  id: "codex",
  name: "Codex",
  models: [
    { id: "gpt-6-codex", name: "GPT-6 Codex", default: true, options: [reasoningLevels(["low", "high"]), tier] },
    { id: "gpt-6-mini", name: "GPT-6 mini", default: false, options: [], images: false },
  ],
});
const grok = provider({ id: "acp:grok", name: "Grok Build", installed: false, detail: "`grok` is not installed", models: [{ id: "grok-4", name: "Grok 4", default: true, options: [] }] });
const providers: ProviderInfo[] = [claude, codex, grok];

function Picker({ onChange, initial = choiceOf("claude"), list = providers, shortcuts = false }: { onChange?: (c: ModelChoice) => void; initial?: ModelChoice; list?: ProviderInfo[]; shortcuts?: boolean }) {
  const [value, setValue] = useState(initial);
  return (
    <ModelPicker
      providers={list}
      value={value}
      shortcuts={shortcuts}
      onChange={(c) => {
        setValue(c);
        onChange?.(c);
      }}
    />
  );
}

const trigger = () => screen.getByRole("button", { name: /^Model:/ });
const open = async () => {
  await userEvent.click(trigger());
  return screen.findByRole("dialog", { name: "Choose a model" });
};
const closed = () => waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());

beforeEach(() => modelPrefs.reset());

describe("the model picker", () => {
  it("a new model keeps the effort and options it takes; another provider starts fresh", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} initial={{ ...choiceOf("claude", "claude-sonnet-4-6"), effort: "high", options: { contextWindow: "1m", thinking: false } }} />, agentsHarness());
    let popup = await open();
    await userEvent.click(within(popup).getByRole("option", { name: /Claude Opus 5.5/ }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: "high", options: {} });
    await userEvent.keyboard("{Escape}");
    await closed();
    popup = await open();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(popup).getByRole("option", { name: /GPT-6 mini/ }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "codex", model: "gpt-6-mini", effort: null, options: {} });
    // No options: picking it closes the popup.
    await closed();
  });

  it("says what a provider needs: installing (its models can't be picked), logging in, an update for newer models", async () => {
    const loggedOut = provider({ ...codex, auth: "missing", detail: "Run `codex login`" });
    renderAgents(<Picker list={[claude, loggedOut, grok]} />, agentsHarness());
    const popup = await open();
    expect(within(popup).getByText("Update Claude Code to v2.1.293 or newer to use Claude Haiku 5.5.")).toBeInTheDocument();
    await userEvent.click(within(popup).getByRole("button", { name: /^Grok Build/ }));
    expect(within(popup).getByRole("button", { name: /^Grok Build/ })).toHaveAccessibleName("Grok Build: Not installed. `grok` is not installed");
    expect(within(popup).getByText("`grok` is not installed")).toBeInTheDocument();
    expect(within(popup).getByRole("option", { name: /Grok 4/ })).toHaveAttribute("aria-disabled", "true");
    await userEvent.click(within(popup).getByRole("button", { name: /^Codex/ }));
    expect(within(popup).getByText(/Codex isn't logged in/)).toBeInTheDocument();
    expect(within(popup).getByText("codex login")).toBeInTheDocument();
    expect(within(within(popup).getByRole("option", { name: /GPT-6 Codex/ })).getByText("Log in first")).toBeInTheDocument();
  });

});

describe("favourites and recent, kept in this browser", () => {
  it("survive a reload and ignore junk", () => {
    modelPrefs.setFavorite("codex:gpt-6-codex", true);
    modelPrefs.reset();
    expect(modelPrefs.get().favorites).toEqual(["codex:gpt-6-codex"]);
    localStorage.setItem("roll:agents:favorite-models", JSON.stringify([1, "x", "codex:gpt-6-codex"]));
    modelPrefs.reset();
    expect(modelPrefs.get().favorites).toEqual(["codex:gpt-6-codex"]);
    localStorage.setItem("roll:agents:favorite-models", "{");
    modelPrefs.reset();
    expect(modelPrefs.get().favorites).toEqual([]);
  });
});

describe("permissions", () => {
  it("names the mode and picks another from a menu", async () => {
    const onChange = vi.fn();
    renderAgents(<PermissionsMenu value="full_access" onChange={onChange} />, agentsHarness());
    await userEvent.click(screen.getByRole("button", { name: "Permissions: Full access" }));
    await userEvent.click(await screen.findByRole("menuitemradio", { name: /Ask/ }));
    expect(onChange).toHaveBeenCalledWith("ask");
  });
});
