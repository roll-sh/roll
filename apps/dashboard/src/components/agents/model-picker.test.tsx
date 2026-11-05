import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { beforeEach, describe, expect, it, vi } from "vitest";
import type { ModelOption, ProviderInfo } from "@/lib/agents-api";
import { reasoningLevels } from "@/lib/agents-model-options";
import { MAX_RECENT, modelPrefs } from "@/lib/agents-models";
import { provider } from "@/test/agents";
import { agentsHarness, renderAgents } from "@/test/agents-render";
import { ModelPicker, PermissionsMenu, choiceOf, updateNotice, type ModelChoice } from "./model-picker";

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
/** The model names listed, as read (the logo, badges and the screen-reader suffix aside). */
const rows = (popup: HTMLElement) =>
  within(popup)
    .queryAllByRole("option")
    .map((o) => {
      const c = o.cloneNode(true) as HTMLElement;
      c.querySelectorAll("[aria-hidden], .sr-only, button, kbd").forEach((x) => x.remove());
      return c.querySelector("[title]")?.getAttribute("title") ?? c.textContent?.trim();
    });

beforeEach(() => modelPrefs.reset());

describe("the model picker", () => {
  it("shows the model's short name, its reasoning level (the default until one is picked) and opens on the provider in use", async () => {
    renderAgents(<Picker />, agentsHarness());
    expect(trigger()).toHaveAccessibleName("Model: Opus 5.5 · Medium");
    // The provider's logo, not a letter.
    expect(trigger().querySelector("svg path")).not.toBeNull();
    const popup = await open();
    expect(within(popup).getByRole("button", { name: "Claude Code" })).toHaveAttribute("aria-pressed", "true");
    // The older model waits under "Show more".
    expect(rows(popup)).toEqual(["Claude Opus 5.5", "Sonnet 5", "Show more (1)"]);
    expect(within(popup).getByRole("combobox", { name: "Search models" })).toHaveFocus();
    await userEvent.click(within(popup).getByRole("option", { name: /Show more/ }));
    expect(rows(popup)).toEqual(["Claude Opus 5.5", "Sonnet 5", "Sonnet 4.6", "Show less"]);
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    expect(rows(popup)).toEqual(["GPT-6 Codex", "GPT-6 mini"]);
    expect(within(within(popup).getByRole("option", { name: /GPT-6 mini/ })).getByText("Text only")).toBeInTheDocument();
  });

  it("lists the reasoning levels the daemon describes, marks the default, and a level closes it", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} />, agentsHarness());
    const popup = await open();
    const levels = within(popup).getByRole("radiogroup", { name: "Reasoning" });
    expect(within(levels).getAllByRole("radio").map((r) => r.textContent)).toEqual(["Low", "MediumDefault", "High", "Max"]);
    expect(within(levels).getByRole("radio", { name: /Medium/ })).toHaveAttribute("aria-checked", "true");
    await userEvent.click(within(levels).getByRole("radio", { name: /High/ }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: "high", options: {} });
    await closed();
    expect(trigger()).toHaveAccessibleName("Model: Opus 5.5 · High");
  });

  it("speed is one click away: Standard | Fast, a bolt on the trigger", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} />, agentsHarness());
    let popup = await open();
    const speed = within(popup).getByRole("group", { name: "Fast Mode" });
    await userEvent.click(within(speed).getByRole("button", { name: "Fast" }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: null, options: { fastMode: true } });
    await userEvent.keyboard("{Escape}");
    await closed();
    expect(trigger()).toHaveAccessibleName("Model: Opus 5.5 · Medium · Fast");
    // Codex's service tiers, the same control.
    popup = await open();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(popup).getByRole("option", { name: /GPT-6 Codex/ }));
    const tiers = within(popup).getByRole("group", { name: "Service Tier" });
    expect(within(tiers).getAllByRole("button").map((b) => b.textContent)).toEqual(["Standard", "Fast"]);
    await userEvent.click(within(tiers).getByRole("button", { name: "Fast" }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "codex", model: "gpt-6-codex", effort: null, options: { serviceTier: "priority" } });
  });

  it("other options sit under the levels: a choice opens a menu, on/off is a switch; the context window shows on the trigger", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} initial={{ ...choiceOf("claude", "claude-sonnet-4-6"), effort: "low" }} />, agentsHarness());
    expect(trigger()).toHaveAccessibleName("Model: Sonnet 4.6 · Low · 200k");
    const popup = await open();
    // The model in use is a legacy one, so its section is open.
    expect(rows(popup)).toContain("Sonnet 4.6");
    await userEvent.click(within(popup).getByRole("button", { name: "Context Window: 200k" }));
    await userEvent.click(await screen.findByRole("menuitemradio", { name: /1M/ }));
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-sonnet-4-6", effort: "low", options: { contextWindow: "1m" } });
    const sw = within(popup).getByRole("switch", { name: "Thinking" });
    expect(sw).toHaveAttribute("aria-checked", "true");
    await userEvent.click(sw);
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-sonnet-4-6", effort: "low", options: { contextWindow: "1m", thinking: false } });
    expect(trigger()).toHaveAccessibleName("Model: Sonnet 4.6 · Low · 1M");
  });

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

  it("pins favourites: a star on the row, first in its provider, and the Favorites tab it opens on", async () => {
    renderAgents(<Picker />, agentsHarness());
    let popup = await open();
    const sonnet = within(popup).getByRole("option", { name: /Sonnet 5/ });
    await userEvent.click(within(sonnet).getByRole("button", { name: "Add to favorites" }));
    // Starring doesn't pick it.
    expect(trigger()).toHaveAccessibleName(/^Model: Opus 5.5/);
    expect(rows(popup).slice(0, 2)).toEqual(["Sonnet 5", "Claude Opus 5.5"]);
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(within(popup).getByRole("option", { name: /GPT-6 mini/ })).getByRole("button", { name: "Add to favorites" }));
    expect(modelPrefs.get().favorites).toEqual(["claude:claude-sonnet-5", "codex:gpt-6-mini"]);
    await userEvent.keyboard("{Escape}");
    await closed();
    popup = await open();
    expect(within(popup).getByRole("button", { name: "Favorites" })).toHaveAttribute("aria-pressed", "true");
    expect(rows(popup)).toEqual(["Sonnet 5", "GPT-6 mini"]);
  });

  it("searches every provider, favourites ranked up, and says so when nothing matches", async () => {
    modelPrefs.setFavorite("codex:gpt-6-mini", true);
    renderAgents(<Picker />, agentsHarness());
    const popup = await open();
    const box = within(popup).getByRole("combobox", { name: "Search models" });
    await userEvent.type(box, "gpt");
    expect(within(popup).queryByRole("toolbar", { name: "Providers" })).toBeNull();
    expect(rows(popup)).toEqual(["GPT-6 mini", "GPT-6 Codex"]);
    await userEvent.clear(box);
    await userEvent.type(box, "haiku");
    // Nothing matches, but an update would bring one.
    expect(within(popup).getByText("No models found")).toBeInTheDocument();
    expect(within(popup).getByText(/to use Claude Haiku 5.5/)).toBeInTheDocument();
  });

  it("keyboard: arrows and Enter pick, ⌘1–9 jump, a model with levels moves to them", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} />, agentsHarness());
    await open();
    await userEvent.keyboard("{ArrowDown}{Enter}");
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-sonnet-5", effort: null, options: {} });
    await closed();
    const popup = await open();
    expect(within(within(popup).getByRole("option", { name: /Claude Opus 5.5/ })).getByText(/1$/)).toBeInTheDocument();
    await userEvent.keyboard("{Control>}1{/Control}");
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: null, options: {} });
    const levels = within(popup).getByRole("radiogroup", { name: "Reasoning" });
    await waitFor(() => expect(within(levels).getByRole("radio", { name: /Medium/ })).toHaveFocus());
    await userEvent.keyboard("{ArrowDown}{Enter}");
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: "high", options: {} });
    await closed();
  });

  it("with shortcuts, ⌘⇧E steps the reasoning level and ⌘⇧M opens it", async () => {
    const onChange = vi.fn();
    renderAgents(<Picker onChange={onChange} shortcuts />, agentsHarness());
    act(() => {
      fireEvent.keyDown(window, { key: "E", metaKey: true, shiftKey: true });
    });
    expect(onChange).toHaveBeenLastCalledWith({ provider: "claude", model: "claude-opus-5-5", effort: "high", options: {} });
    expect(trigger()).toHaveAccessibleName("Model: Opus 5.5 · High");
    act(() => {
      fireEvent.keyDown(window, { key: "M", ctrlKey: true, shiftKey: true });
    });
    expect(await screen.findByRole("dialog", { name: "Choose a model" })).toBeInTheDocument();
  });

  it("remembers what was used last under Recent", async () => {
    renderAgents(<Picker />, agentsHarness());
    let popup = await open();
    expect(within(popup).queryByRole("button", { name: "Recent" })).toBeNull();
    await userEvent.click(within(popup).getByRole("button", { name: "Codex" }));
    await userEvent.click(within(popup).getByRole("option", { name: /GPT-6 mini/ }));
    await closed();
    popup = await open();
    await userEvent.click(within(popup).getByRole("button", { name: "Recent" }));
    expect(rows(popup)).toEqual(["GPT-6 mini"]);
  });
});

describe("update notices", () => {
  it("name the newest version needed and every model, filtered by a search", () => {
    const p = provider({
      name: "Claude Code",
      update_required: [
        { id: "a", name: "Haiku 5.5", min_version: "2.1.293" },
        { id: "b", name: "Opus 6", min_version: "2.1.310" },
        { id: "c", name: "Sonnet 6", min_version: "2.1.300" },
      ],
    });
    expect(updateNotice(p)).toBe("Update Claude Code to v2.1.310 or newer to use Haiku 5.5, Opus 6, and Sonnet 6.");
    expect(updateNotice(p, "haiku")).toBe("Update Claude Code to v2.1.293 or newer to use Haiku 5.5.");
    expect(updateNotice(p, "zzz")).toBeNull();
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

describe("models an endpoint lists", () => {
  it("say which endpoint they come from, beside the Custom tag", async () => {
    const gemini = provider({
      id: "acp:gemini",
      name: "Gemini CLI",
      models: [
        { id: "gemini-2.5-pro", name: "Gemini 2.5 Pro", default: true, options: [] },
        { id: "gemini-3-flash", name: "gemini-3-flash", default: false, options: [], via: "CLIProxyAPI" },
      ],
    });
    renderAgents(<Picker list={[gemini]} initial={choiceOf("acp:gemini")} />, agentsHarness());
    const popup = await open();
    expect(within(within(popup).getByRole("option", { name: /gemini-3-flash/ })).getByText("via CLIProxyAPI")).toBeInTheDocument();
    expect(within(within(popup).getByRole("option", { name: /Gemini 2\.5 Pro/ })).queryByText(/^via /)).toBeNull();
  });
});
