import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ProviderInfo } from "@/lib/agents-api";
import { MAX_RECENT, modelPrefs } from "@/lib/agents-models";
import { provider } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { ModelPicker, PermissionsMenu, type ModelChoice } from "./model-picker";

const codex = provider({
  id: "codex",
  name: "Codex",
  models: [
    { id: "gpt-6-codex", name: "GPT-6 Codex", default: true, efforts: ["low", "high"] },
    { id: "gpt-6-mini", name: "GPT-6 mini", default: false, efforts: [] },
  ],
});
const providers: ProviderInfo[] = [provider(), codex];

function Picker({ onChange, initial = { provider: "claude", model: null, effort: null } }: { onChange?: (c: ModelChoice) => void; initial?: ModelChoice }) {
  const [value, setValue] = useState(initial);
  return (
    <ModelPicker
      providers={providers}
      value={value}
      onChange={(c) => {
        setValue(c);
        onChange?.(c);
      }}
    />
  );
}

const open = async () => {
  await userEvent.click(screen.getByRole("button", { name: /^Model:/ }));
  return screen.findByRole("dialog", { name: "Choose a model" });
};
/** The model names listed, as read (the provider's mark and the screen-reader suffix aside). */
const rows = (popup: HTMLElement) =>
  within(popup)
    .queryAllByRole("option")
    .map((o) => {
      const c = o.cloneNode(true) as HTMLElement;
      c.querySelectorAll("[aria-hidden], .sr-only, button").forEach((x) => x.remove());
      return c.textContent?.trim();
    });

beforeEach(() => modelPrefs.reset());

describe("the model picker", () => {
  it("shows the model and its effort, and opens on the provider in use with every model of it", async () => {
    renderAgents(<Picker initial={{ provider: "claude", model: "claude-opus-5-5", effort: "high" }} />, agentsHarness());
    expect(screen.getByRole("button", { name: "Model: Opus 5.5 · high" })).toBeInTheDocument();
    const popup = await open();
    expect(within(popup).getByRole("button", { name: "Claude Code" })).toHaveAttribute("aria-pressed", "true");
    expect(rows(popup)).toEqual(["Opus 5.5", "Sonnet 5"]);
    expect(within(popup).getByRole("combobox", { name: "Search models" })).toHaveFocus();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    expect(rows(popup)).toEqual(["GPT-6 Codex", "GPT-6 mini"]);
  });

  it("pins favourites: a star on the row, first in its provider, and the Favorites tab it opens on", async () => {
    renderAgents(<Picker />, agentsHarness());
    let popup = await open();
    const sonnet = within(popup).getByRole("option", { name: /Sonnet 5/ });
    await userEvent.click(within(sonnet).getByRole("button", { name: "Add to favorites" }));
    // Starring doesn't pick it.
    expect(screen.getByRole("button", { name: /^Model: Opus 5.5/ })).toBeInTheDocument();
    expect(rows(popup)).toEqual(["Sonnet 5", "Opus 5.5"]);
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(within(popup).getByRole("option", { name: /GPT-6 mini/ })).getByRole("button", { name: "Add to favorites" }));
    expect(modelPrefs.get().favorites).toEqual(["claude:claude-sonnet-5", "codex:gpt-6-mini"]);
    await userEvent.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());
    popup = await open();
    expect(within(popup).getByRole("button", { name: "Favorites" })).toHaveAttribute("aria-pressed", "true");
    expect(rows(popup)).toEqual(["Sonnet 5", "GPT-6 mini"]);
    await userEvent.click(within(within(popup).getByRole("option", { name: /Sonnet 5/ })).getByRole("button", { name: "Remove from favorites" }));
    expect(modelPrefs.get().favorites).toEqual(["codex:gpt-6-mini"]);
  });

  it("searches every provider, favourites ranked up, and says so when nothing matches", async () => {
    modelPrefs.setFavorite("codex:gpt-6-mini", true);
    renderAgents(<Picker />, agentsHarness());
    const popup = await open();
    const box = within(popup).getByRole("combobox", { name: "Search models" });
    await userEvent.type(box, "gpt");
    // The sidebar steps aside while searching.
    expect(within(popup).queryByRole("toolbar", { name: "Providers" })).toBeNull();
    expect(rows(popup)).toEqual(["GPT-6 mini", "GPT-6 Codex"]);
    await userEvent.clear(box);
    await userEvent.type(box, "son 5");
    expect(rows(popup)).toEqual(["Sonnet 5"]);
    await userEvent.clear(box);
    await userEvent.type(box, "zzz");
    expect(within(popup).getByText("No models found")).toBeInTheDocument();
  });

  it("keyboard: arrows and Enter pick, a model with effort levels moves to them, a level closes it", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} />, agentsHarness());
    await open();
    await userEvent.keyboard("{ArrowDown}{Enter}");
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-sonnet-5", effort: null });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());
    const popup = await open();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(popup).getByRole("option", { name: /GPT-6 Codex/ }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "codex", model: "gpt-6-codex", effort: null });
    const levels = within(popup).getByRole("radiogroup", { name: "Effort" });
    await waitFor(() => expect(within(levels).getByRole("radio", { name: "Default" })).toHaveFocus());
    await userEvent.keyboard("{ArrowDown}{ArrowDown}{Enter}");
    expect(onChange).toHaveBeenLastCalledWith({ provider: "codex", model: "gpt-6-codex", effort: "high" });
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());
    expect(screen.getByRole("button", { name: "Model: GPT-6 Codex · high" })).toBeInTheDocument();
  });

  it("remembers what was used last under Recent", async () => {
    renderAgents(<Picker />, agentsHarness());
    let popup = await open();
    expect(within(popup).queryByRole("button", { name: "Recent" })).toBeNull();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(popup).getByRole("option", { name: /GPT-6 mini/ }));
    await waitFor(() => expect(screen.queryByRole("dialog", { name: "Choose a model" })).toBeNull());
    popup = await open();
    await userEvent.click(within(popup).getByRole("button", { name: "Recent" }));
    expect(rows(popup)).toEqual(["GPT-6 mini"]);
  });
});

describe("favourites and recent, kept in this browser", () => {
  it("set the wanted state (not a toggle); recent ones are newest first and bounded", () => {
    modelPrefs.setFavorite("claude:claude-opus-5-5", true);
    modelPrefs.setFavorite("claude:claude-opus-5-5", true);
    expect(modelPrefs.get().favorites).toEqual(["claude:claude-opus-5-5"]);
    modelPrefs.setFavorite("claude:claude-opus-5-5", false);
    expect(modelPrefs.get().favorites).toEqual([]);
    for (let i = 0; i < MAX_RECENT + 2; i++) modelPrefs.used(`claude:m${i}`);
    modelPrefs.used("claude:m3");
    const recent = modelPrefs.get().recent;
    expect(recent).toHaveLength(MAX_RECENT);
    expect(recent[0]).toBe("claude:m3");
    expect(new Set(recent).size).toBe(recent.length);
  });

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
