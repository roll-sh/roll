import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command, EndpointInfo, EndpointProfile, Machine, MachineSettings, Reply } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { setEndpoint } from "@/lib/agents-endpoints";
import { agentsSurface, resetAgentsSurface, viewFromHash, hashFor } from "@/lib/agents-surface";
import { modelPrefs } from "@/lib/agents-models";
import { DEFAULT_OWNER_SETTINGS, parseOwnerSettings } from "@/lib/owner-settings";
import { machine, machineConfig, project, provider, SEAL_KEY, T0, thread } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { MarkStateProvider } from "../mark-state";
import { AgentsControl } from "./agents-control";
import { Composer } from "./composer";
import { SettingsPanel } from "./settings-panel";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

const configures = (h: AgentsHarness) =>
  h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: "machine.configure" }> => c.type === "machine.configure").map((c) => c.settings);

/** A machine whose daemon reports settings, and answers `machine.configure` as a daemon does: its `hello` (the machine record) first, then the reply. */
function configured(set: MachineSettings = {}, over: Partial<Machine> = {}, refuse?: string) {
  const codex = provider({ id: "codex", name: "Codex", auth: "missing", models: [{ id: "gpt-5.5-codex", name: "GPT-5.5 Codex", default: true, options: [] }] });
  const m = machine({ info: { ...machine().info!, providers: [provider(), codex], settings: machineConfig(set) }, ...over });
  let cursor = 10;
  const h: AgentsHarness = agentsHarness({
    machines: [m],
    reply: (c: Command): Reply => {
      if (c.type === "terminal.open") return { type: "created", id: "trm_9" };
      if (c.type !== "machine.configure") return { type: "ok" };
      if (refuse) return { type: "error", code: "invalid", message: refuse };
      const providers = [provider(), codex].filter((p) => c.settings.providers?.[p.id]?.enabled !== false);
      h.client.enqueue({ type: "machine", cursor: ++cursor, machine: { ...m, info: { ...m.info!, providers, settings: machineConfig(c.settings) } } });
      return { type: "ok" };
    },
    ctx: { services: fakeServices() },
  });
  renderAgents(<SettingsPanel target={`m:${m.id}`} onClose={vi.fn()} />, h);
  return { h, m };
}

beforeEach(() => {
  resetAgentsSurface();
  window.history.replaceState(null, "", "/dashboard");
});
afterEach(() => setActiveClient(null));

describe("a machine's settings", () => {
  it("lists its providers with their state, and turns one off and on again", async () => {
    const { h } = configured();
    const providers = screen.getByRole("region", { name: "Providers" });
    expect(within(providers).getByText("2.1.310 · logged in")).toBeInTheDocument();
    expect(within(providers).getByText("2.1.310 · needs a login")).toBeInTheDocument();
    const codex = within(providers).getByRole("switch", { name: "Offer Codex" });
    expect(codex).toBeChecked();
    await userEvent.click(codex);
    await waitFor(() => expect(configures(h)).toEqual([{ providers: { codex: { enabled: false } } }]));
    // The daemon's hello says it's off: no longer offered, and the card says so.
    await waitFor(() => expect(within(providers).getByRole("switch", { name: "Offer Codex" })).not.toBeChecked());
    expect(within(providers).getByText("Off")).toBeInTheDocument();
    await userEvent.click(within(providers).getByRole("switch", { name: "Offer Codex" }));
    await waitFor(() => expect(configures(h)[1]).toEqual({}));
  });

  it("logs a provider in from its card, in a terminal on the canvas", async () => {
    const { h } = configured();
    await userEvent.click(screen.getByRole("button", { name: "Log in" }));
    await waitFor(() => expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), { type: "terminal.open", command: "codex login", cols: 100, rows: 26 }));
    expect(h.focus).toHaveBeenCalledWith("x:trm_9");
  });

  it("sets a provider's binary, arguments, environment and custom models, keeping the rest", async () => {
    const { h } = configured({ branch_prefix: "ada" });
    await userEvent.click(screen.getByRole("button", { name: "Claude Code details" }));
    const binary = screen.getByRole("textbox", { name: "Claude Code binary" });
    await userEvent.type(binary, "/opt/claude/bin/claude{Enter}");
    await waitFor(() => expect(configures(h)).toHaveLength(1));
    expect(configures(h)[0]).toEqual({ branch_prefix: "ada", providers: { claude: { binary: "/opt/claude/bin/claude" } } });

    await userEvent.type(screen.getByRole("textbox", { name: "New Claude Code argument" }), "--verbose{Enter}");
    await waitFor(() => expect(configures(h)[1]?.providers?.claude).toEqual({ binary: "/opt/claude/bin/claude", args: ["--verbose"] }));

    // config.toml's variables show by name; their values never come here.
    const env = screen.getByRole("list", { name: "Claude Code environment" });
    expect(within(env).getByText("ANTHROPIC_API_KEY")).toBeInTheDocument();
    expect(within(env).getByText("set in config.toml")).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox", { name: "Variable name" }), "CLAUDE_CONFIG_DIR");
    await userEvent.type(screen.getByRole("textbox", { name: "Variable value" }), "~/.claude-work{Enter}");
    await waitFor(() => expect(configures(h)[2]?.providers?.claude?.env).toEqual({ CLAUDE_CONFIG_DIR: "~/.claude-work" }));

    // T3's custom-model rules, then one added.
    const slug = screen.getByRole("textbox", { name: "Claude Code model slug" });
    await userEvent.type(slug, "claude-opus-5-5{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent("That model is already built in.");
    await userEvent.clear(slug);
    await userEvent.type(slug, "claude-opus-5-5-1m");
    await userEvent.type(screen.getByRole("textbox", { name: "Claude Code model name" }), "Opus 1M{Enter}");
    await waitFor(() => expect(configures(h)[3]?.providers?.claude?.models).toEqual([{ id: "claude-opus-5-5-1m", name: "Opus 1M" }]));
    await userEvent.type(screen.getByRole("textbox", { name: "Claude Code model slug" }), "claude-opus-5-5-1m{Enter}");
    expect(screen.getByRole("alert")).toHaveTextContent("That custom model is already saved.");
    expect(configures(h)).toHaveLength(4);
  });

  it("sets Claude's auto-compact window and when idle threads compact, over the machine's default (T3's Claude settings)", async () => {
    const { h } = configured();
    // The machine's default, for providers that keep a prompt cache.
    await userEvent.click(screen.getByRole("combobox", { name: "Compact idle threads" }));
    await userEvent.click(await screen.findByRole("option", { name: "Before the cache expires (beta)" }));
    await waitFor(() => expect(configures(h)[0]).toEqual({ idle_compaction: "before_cache_expires" }));

    await userEvent.click(screen.getByRole("button", { name: "Claude Code details" }));
    const window = screen.getByRole("textbox", { name: "Claude Code auto-compact after" });
    expect(window).toHaveAttribute("placeholder", "e.g. 300000");
    await userEvent.type(window, "50000{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("Compact after 100,000 to 1,000,000 tokens. Leave empty to use Claude's default.");
    expect(configures(h)).toHaveLength(1);
    await userEvent.clear(window);
    await userEvent.type(window, "300,000{Enter}");
    await waitFor(() => expect(configures(h)[1]?.providers?.claude).toEqual({ auto_compact_window: 300_000 }));

    // Claude's own choice, with "Use default" saying what the machine's is.
    const idle = screen.getByRole("combobox", { name: "Claude Code: compact idle threads" });
    expect(idle).toHaveTextContent("Use default (Before the cache expires (beta))");
    await userEvent.click(idle);
    await userEvent.click(await screen.findByRole("option", { name: "Never" }));
    await waitFor(() => expect(configures(h)[2]?.providers?.claude).toEqual({ auto_compact_window: 300_000, idle_compaction: "off" }));
    // Codex has neither: its compaction isn't Claude's knob.
    await userEvent.click(screen.getByRole("button", { name: "Codex details" }));
    expect(screen.queryByRole("textbox", { name: "Codex auto-compact after" })).toBeNull();
  });

  it("never sends a credential: the panel says to set it in config.toml on the machine", async () => {
    const { h } = configured();
    await userEvent.click(screen.getByRole("button", { name: "Claude Code details" }));
    for (const name of ["ANTHROPIC_API_KEY", "ANTHROPIC_BASE_URL", "HTTPS_PROXY"]) {
      await userEvent.clear(screen.getByRole("textbox", { name: "Variable name" }));
      await userEvent.type(screen.getByRole("textbox", { name: "Variable name" }), name);
      await userEvent.type(screen.getByRole("textbox", { name: "Variable value" }), "sk-ant-secret{Enter}");
      expect(screen.getByRole("alert")).toHaveTextContent(`${name} may hold a credential or an address`);
      expect(screen.getByRole("alert")).toHaveTextContent("/Users/ada/.local/share/roll/agent/config.toml");
      await userEvent.clear(screen.getByRole("textbox", { name: "Variable value" }));
    }
    expect(configures(h)).toEqual([]);
    expect(screen.getByText(/roll never relays a credential/)).toBeInTheDocument();
  });

  it("sets worktrees, git, projects and delegation, with a reset back to what applies", async () => {
    const { h } = configured({ pull_every: 300 });
    expect(screen.getByRole("textbox", { name: "Worktree folder" })).toHaveAttribute("placeholder", "/Users/ada/.local/share/roll/agent/worktrees");
    await userEvent.type(screen.getByRole("textbox", { name: "Branch prefix" }), "ada/wip{Enter}");
    await waitFor(() => expect(configures(h)[0]).toEqual({ pull_every: 300, branch_prefix: "ada/wip" }));
    // A prefix git can't take is refused before it's sent.
    await userEvent.clear(screen.getByRole("textbox", { name: "Branch prefix" }));
    await userEvent.type(screen.getByRole("textbox", { name: "Branch prefix" }), "a..b{Enter}");
    expect(await screen.findByRole("alert")).toHaveTextContent("can't start a branch name");
    expect(configures(h)).toHaveLength(1);

    await userEvent.type(screen.getByRole("textbox", { name: "Git name" }), "Ada Lovelace");
    expect(screen.getByText("Both a name and an email, or neither.")).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox", { name: "Git email" }), "ada@example.com{Enter}");
    await waitFor(() => expect(configures(h)[1]).toMatchObject({ git_name: "Ada Lovelace", git_email: "ada@example.com" }));

    await userEvent.click(screen.getByRole("combobox", { name: "Subagent depth" }));
    await userEvent.click(await screen.findByRole("option", { name: "1 deep" }));
    await waitFor(() => expect(configures(h)[2]).toMatchObject({ max_depth: 1 }));
    await userEvent.click(screen.getByRole("button", { name: "Reset to default: Auto-pull checks" }));
    await waitFor(() => expect(configures(h)[3]).not.toHaveProperty("pull_every"));
  });

  it("finds settings by search, opening the provider that holds one", async () => {
    configured();
    const search = screen.getByRole("searchbox", { name: "Search settings" });
    await userEvent.type(search, "branch");
    expect(screen.getByRole("textbox", { name: "Branch prefix" })).toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Providers" })).not.toBeInTheDocument();
    expect(screen.queryByRole("region", { name: "Machine" })).not.toBeInTheDocument();
    await userEvent.clear(search);
    await userEvent.type(search, "codex env");
    const providers = screen.getByRole("region", { name: "Providers" });
    expect(within(providers).getByRole("button", { name: "Codex details" })).toHaveAttribute("aria-expanded", "true");
    expect(within(providers).queryByRole("button", { name: "Claude Code details" })).not.toBeInTheDocument();
    expect(within(providers).getByRole("textbox", { name: "Variable name" })).toBeInTheDocument();
    expect(within(providers).queryByRole("textbox", { name: "Codex binary" })).not.toBeInTheDocument();
    await userEvent.clear(search);
    await userEvent.type(search, "xylophone");
    expect(screen.getByText("No settings found")).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(search).toHaveValue("");
    expect(screen.getByRole("region", { name: "Providers" })).toBeInTheDocument();
  });

  it("says why the daemon refused a change", async () => {
    configured({}, {}, "no provider acp:nope on this machine");
    await userEvent.click(screen.getByRole("switch", { name: "Offer Codex" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("no provider acp:nope on this machine");
  });

  it("says a provider is being checked while its daemon probes it", () => {
    configured({}, { info: { ...machine().info!, providers: [provider({ checking: true })], settings: machineConfig() } });
    const providers = screen.getByRole("region", { name: "Providers" });
    expect(within(providers).getByText("Checking…").className).toContain("shimmer");
    expect(within(providers).queryByText("2.1.310 · logged in")).not.toBeInTheDocument();
  });

  it("can't change an offline machine's settings, nor an old daemon's, and says so", () => {
    configured({}, { online: false });
    expect(screen.getByText(/ada-mbp is offline: its settings change once it's back/)).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Offer Codex" })).toHaveAttribute("aria-disabled", "true");
    expect(screen.getByRole("textbox", { name: "Branch prefix" })).toBeDisabled();
  });

  it("an old daemon's machine shows its providers, read-only", () => {
    const h = agentsHarness({ ctx: { services: fakeServices() } });
    renderAgents(<SettingsPanel target="m:mch_1" onClose={vi.fn()} />, h);
    expect(screen.getByText(/too old to change its settings from here/)).toBeInTheDocument();
    expect(screen.getByRole("switch", { name: "Offer Claude Code" })).toHaveAttribute("aria-disabled", "true");
  });

  it("renames the machine and removes it after asking", async () => {
    const services = fakeServices();
    const onClose = vi.fn();
    const h = agentsHarness({ machines: [machine({ info: { ...machine().info!, settings: machineConfig() } })], ctx: { services } });
    renderAgents(<SettingsPanel target="m:mch_1" onClose={onClose} />, h);
    const name = screen.getByRole("textbox", { name: "Machine name" });
    await userEvent.clear(name);
    await userEvent.type(name, "ada-studio{Enter}");
    await waitFor(() => expect(services.machines.rename).toHaveBeenCalledWith("acme", "mch_1", "ada-studio"));
    await userEvent.click(screen.getByRole("button", { name: "Remove" }));
    expect(services.machines.remove).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Remove ada-mbp" }));
    await waitFor(() => expect(services.machines.remove).toHaveBeenCalledWith("acme", "mch_1"));
    expect(onClose).toHaveBeenCalled();
  });
});

describe("the owner's agents settings", () => {
  function owner(services = fakeServices()) {
    const h = agentsHarness({ machines: [machine({ info: { ...machine().info!, settings: machineConfig() } })], ctx: { services } });
    conn = { client: h.client, services, error: null };
    renderAgents(
      <MarkStateProvider>
        <AgentsControl />
      </MarkStateProvider>,
      h,
    );
    return { h, services };
  }

  it("opens from the agents header's gear and ⌘,, at #agents/settings/owner", async () => {
    owner();
    expect(viewFromHash("#agents/settings/owner")).toEqual({ kind: "settings", target: "owner" });
    expect(hashFor({ kind: "settings", target: "owner" })).toBe("#agents/settings/owner");
    await userEvent.keyboard("{Meta>},{/Meta}");
    expect(agentsSurface.get()).toMatchObject({ open: true, view: { kind: "settings", target: "owner" } });
    const panel = await screen.findByRole("region", { name: "General" });
    expect(within(panel).getByText("Notifications")).toBeInTheDocument();
    expect(window.location.hash).toBe("#agents/settings/owner");
    await userEvent.click(within(screen.getByRole("complementary", { name: "Settings" })).getByRole("button", { name: "Close (Esc)" }));
    expect(agentsSurface.get().view).toEqual({ kind: "canvas" });
    await userEvent.click(screen.getByRole("button", { name: "Agents settings (⌘,)" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "owner" });
    // A machine's settings are one click away.
    await userEvent.click(screen.getByRole("button", { name: "ada-mbp settings" }));
    expect(agentsSurface.get().view).toEqual({ kind: "settings", target: "m:mch_1" });
  });

  it("saves each change at once through roll, keeping keys it doesn't know, over the revision it has", async () => {
    const services = fakeServices();
    await services.settings.save("acme", { send: "enter", favorites: ["claude/opus"] });
    owner(services);
    await userEvent.keyboard("{Meta>},{/Meta}");
    const chat = await screen.findByRole("region", { name: "Chat" });
    await userEvent.click(within(within(chat).getByRole("group", { name: "Send with" })).getByRole("button", { name: "⌘/Ctrl+Enter" }));
    await waitFor(() => expect(services.settings.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ send: "mod_enter", favorites: ["claude/opus"] }), 1));
    // Another device saved meanwhile: the change goes over theirs.
    await services.settings.save("acme", { send: "mod_enter", favorites: ["claude/opus"], follow_up: "steer" });
    await userEvent.click(screen.getByRole("switch", { name: "Confirm before deleting a thread" }));
    await waitFor(() => expect(services.settings.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ confirm_delete: false, follow_up: "steer" }), 3));
    expect(screen.getByRole("switch", { name: "Confirm before deleting a thread" })).not.toBeChecked();
    expect(within(within(chat).getByRole("group", { name: "While a run is going" })).getByRole("button", { name: "Steer" })).toHaveAttribute("aria-pressed", "true");
    await userEvent.click(screen.getByRole("button", { name: "Restore defaults" }));
    await userEvent.click(screen.getByRole("button", { name: "Restore" }));
    await waitFor(() => expect(services.settings.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ send: "enter", confirm_delete: true, favorites: ["claude/opus"] }), 4));
  });

  it("keeps the model picker's favourites and recent models with them: this browser's go there once, then roll's copy wins", async () => {
    localStorage.clear();
    modelPrefs.reset();
    modelPrefs.setFavorite("claude:claude-opus-5-5", true);
    modelPrefs.used("claude:claude-sonnet-5");
    const services = fakeServices();
    await services.settings.save("acme", { send: "enter" });
    owner(services);
    await userEvent.keyboard("{Meta>},{/Meta}");
    // Never synced: this browser's are kept by roll (the migration from before).
    await waitFor(() =>
      expect(services.settings.save).toHaveBeenLastCalledWith(
        "acme",
        expect.objectContaining({ send: "enter", favorite_models: ["claude:claude-opus-5-5"], recent_models: ["claude:claude-sonnet-5"] }),
        1,
      ),
    );
    // A pick in the picker goes there too.
    modelPrefs.setFavorite("codex:gpt-5.5-codex", true);
    await waitFor(() => expect(services.settings.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ favorite_models: ["claude:claude-opus-5-5", "codex:gpt-5.5-codex"] }), 2));
    expect(JSON.parse(localStorage.getItem("roll:agents:favorite-models") ?? "[]")).toEqual(["claude:claude-opus-5-5", "codex:gpt-5.5-codex"]);
    modelPrefs.reset();
  });

  it("another device's favourites replace this browser's cached ones", async () => {
    localStorage.clear();
    modelPrefs.reset();
    modelPrefs.setFavorite("claude:claude-haiku-5", true);
    const services = fakeServices();
    await services.settings.save("acme", { favorite_models: ["codex:gpt-5.5"], recent_models: [] });
    owner(services);
    await userEvent.keyboard("{Meta>},{/Meta}");
    await waitFor(() => expect(modelPrefs.get().favorites).toEqual(["codex:gpt-5.5"]));
    // The owner's copy loaded over the cache: nothing to save back.
    expect(services.settings.save).toHaveBeenCalledTimes(1);
    expect(JSON.parse(localStorage.getItem("roll:agents:favorite-models") ?? "[]")).toEqual(["codex:gpt-5.5"]);
    modelPrefs.reset();
  });

  it("says why a save failed", async () => {
    const services = fakeServices({ settings: { save: vi.fn(async () => Promise.reject(new Error("roll is down"))) } });
    owner(services);
    await userEvent.keyboard("{Meta>},{/Meta}");
    await userEvent.click(await screen.findByRole("switch", { name: "Confirm before deleting a thread" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("roll is down");
  });
});

describe("the model picker's lists follow the owner", () => {
  it("connects: never synced takes this browser's, then the owner's copy wins, except over a save on its way", async () => {
    localStorage.clear();
    modelPrefs.reset();
    modelPrefs.setFavorite("claude:claude-opus-5-5", true);
    const saved: unknown[] = [];
    let release = () => {};
    const link = { save: vi.fn((p: unknown) => new Promise<void>((done) => ((release = done), saved.push(p)))) };
    const off = modelPrefs.connect(null, link);
    expect(saved).toEqual([{ favorites: ["claude:claude-opus-5-5"], recent: [] }]);
    // The owner's older copy coming back while that save is on its way doesn't undo it.
    modelPrefs.connect({ favorites: [], recent: [] }, link);
    expect(modelPrefs.get().favorites).toEqual(["claude:claude-opus-5-5"]);
    release();
    await Promise.resolve();
    await Promise.resolve();
    modelPrefs.connect({ favorites: ["codex:gpt-5.5"], recent: ["codex:gpt-5.5"] }, link);
    expect(modelPrefs.get()).toEqual({ favorites: ["codex:gpt-5.5"], recent: ["codex:gpt-5.5"] });
    expect(link.save).toHaveBeenCalledTimes(1);
    // Let go: changes stay in this browser.
    off();
    modelPrefs.used("claude:claude-sonnet-5");
    expect(link.save).toHaveBeenCalledTimes(1);
    expect(parseOwnerSettings({ favorite_models: ["codex:gpt-5.5", "bad", 7], recent_models: "x" })).toMatchObject({ favorite_models: ["codex:gpt-5.5"], recent_models: null });
    modelPrefs.reset();
  });
});

describe("what the settings change", () => {
  it("the send key: with ⌘/Ctrl+Enter, Enter breaks the line", async () => {
    const onSubmit = vi.fn(async () => {});
    const h = agentsHarness({ ctx: { prefs: { settings: { ...DEFAULT_OWNER_SETTINGS, send: "mod_enter" }, revision: 1, loaded: true, error: null, save: vi.fn() } } });
    renderAgents(<Composer mode={{ kind: "new", project: projectFixture(), machine: machine() }} onSubmit={onSubmit} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "line one{Enter}line two");
    expect(onSubmit).not.toHaveBeenCalled();
    expect(box).toHaveValue("line one\nline two");
    await userEvent.keyboard("{Control>}{Enter}{/Control}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ prompt: "line one\nline two" })));
  });

  it("while a run is going, the owner's Queue swaps what Enter and ⌘/Ctrl+Enter do", async () => {
    const h = agentsHarness({ ctx: { prefs: { settings: { ...DEFAULT_OWNER_SETTINGS, follow_up: "queue" }, revision: 1, loaded: true, error: null, save: vi.fn() } } });
    renderAgents(<Composer mode={{ kind: "thread", thread: thread({ status: "running", active_run: "run_1" }), machine: machine() }} />, h);
    const sent = () => h.command.mock.calls.map((c) => c[2]).at(-1);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "after this{Enter}");
    await waitFor(() => expect(sent()).toMatchObject({ type: "message.send", text: "after this", mode: "queue" }));
    await userEvent.type(box, "right now");
    fireEvent.keyDown(box, { key: "Enter", ctrlKey: true });
    await waitFor(() => expect(sent()).toMatchObject({ text: "right now", mode: "steer" }));
  });

  it("new threads start from the machine's defaults, then the owner's, after the project's", async () => {
    const onSubmit = vi.fn(async () => {});
    const m = machine({
      info: { ...machine().info!, providers: [provider(), provider({ id: "codex", name: "Codex", models: [] })], settings: machineConfig({ defaults: { runtime_mode: "ask" } }) },
    });
    const prefs = { settings: { ...DEFAULT_OWNER_SETTINGS, defaults: { provider: "codex", interaction_mode: "plan" as const } }, revision: 1, loaded: true, error: null, save: vi.fn() };
    renderAgents(<Composer mode={{ kind: "new", project: projectFixture(), machine: m }} onSubmit={onSubmit} />, agentsHarness({ ctx: { prefs } }));
    expect(screen.getByRole("button", { name: /^Model: Codex/ })).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "go{Enter}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ provider: "codex", runtime_mode: "ask", interaction_mode: "plan" })));
  });
});

function projectFixture() {
  return project({ repo: null });
}

// ── endpoints (docs/code/agents.md, "Endpoints") ──

const KEY = "sk-live-do-not-show-9f3a";
const PROXY: EndpointInfo = {
  id: "cliproxy",
  name: "CLIProxyAPI",
  kind: "cliproxy",
  base_url: "http://proxy.local:8317",
  providers: ["claude"],
  key: "sealed",
  models: [{ id: "gemini-2.5-pro" }, { id: "gpt-5.5" }],
  checked_at: Date.now() - 3_600_000,
};

const sentOf = <T extends Command["type"]>(h: AgentsHarness, type: T) =>
  h.command.mock.calls.map((c) => c[2]).filter((c): c is Extract<Command, { type: T }> => c.type === type);

/** A machine with a seal key whose daemon keeps endpoints as `endpoint.set`/`remove` say, and says `hello` again. */
function withEndpoints(endpoints: EndpointInfo[] = [], over: Partial<Machine> = {}, refuse?: string) {
  const codex = provider({ id: "codex", name: "Codex", models: [] });
  let m = machine({ info: { ...machine().info!, seal_key: SEAL_KEY, providers: [provider(), codex], settings: machineConfig({}, { endpoints }) }, ...over });
  let cursor = 10;
  const hello = (list: EndpointInfo[]) => {
    m = { ...m, info: { ...m.info!, settings: { ...m.info!.settings!, endpoints: list } } };
    h.client.enqueue({ type: "machine", cursor: ++cursor, machine: m });
  };
  const h: AgentsHarness = agentsHarness({
    machines: [m],
    reply: (c: Command): Reply => {
      const list = m.info?.settings?.endpoints ?? [];
      if (c.type === "endpoint.set") {
        if (refuse) return { type: "error", code: "invalid", message: refuse };
        const old = list.find((e) => e.id === c.endpoint.id);
        const key = c.endpoint.key_ref ? "reference" : c.key?.type === "set" ? "sealed" : c.key?.type === "clear" ? "none" : (old?.key ?? "none");
        hello(setEndpoint(list, { ...c.endpoint, key, models: old?.models ?? [] }));
        return { type: "ok" };
      }
      if (c.type === "endpoint.remove") {
        hello(list.filter((e) => e.id !== c.endpoint));
        return { type: "ok" };
      }
      if (c.type === "endpoint.check") return { type: "endpoint_checked", ok: true, models: 42, ms: 180 };
      return { type: "ok" };
    },
    ctx: { services: fakeServices() },
  });
  renderAgents(<SettingsPanel target={`m:${m.id}`} onClose={vi.fn()} />, h);
  return { h };
}

const endpointsRegion = () => screen.getByRole("region", { name: "Endpoints" });

describe("a machine's endpoints", () => {
  it("adds one with its key sealed to the machine, and the typed key is nowhere afterwards", async () => {
    const { h } = withEndpoints();
    expect(within(endpointsRegion()).getByText(/No endpoints/)).toBeInTheDocument();
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New endpoint" });
    expect(within(form).getByRole("textbox", { name: "Base URL" })).toHaveAttribute("placeholder", "http://127.0.0.1:8317");
    await userEvent.type(within(form).getByRole("textbox", { name: "Endpoint name" }), "Team proxy");
    await userEvent.type(within(form).getByRole("textbox", { name: "Base URL" }), "http://proxy.local:8317/");
    await userEvent.click(within(form).getByRole("checkbox", { name: /Claude Code/ }));
    const key = within(form).getByLabelText("Key");
    expect(key).toHaveAttribute("type", "password");
    expect(key).toHaveAttribute("autocomplete", "off");
    await userEvent.type(key, KEY);
    await userEvent.click(within(form).getByRole("button", { name: "Add endpoint" }));

    await waitFor(() => expect(sentOf(h, "endpoint.set")).toHaveLength(1));
    const [set] = sentOf(h, "endpoint.set");
    expect(set.endpoint).toEqual({ id: "team-proxy", name: "Team proxy", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude"] });
    expect(set.key).toEqual({ type: "set", sealed: { v: 1, epk: expect.stringMatching(/^[\w-]{43}$/), nonce: expect.any(String), ct: expect.any(String) } });
    // Only the box went out; the editor closed and the key is gone from the page.
    expect(JSON.stringify(h.command.mock.calls)).not.toContain(KEY);
    await waitFor(() => expect(within(endpointsRegion()).queryByRole("form")).toBeNull());
    expect(document.body.innerHTML).not.toContain(KEY);
    const item = within(endpointsRegion()).getByRole("group", { name: "Endpoint Team proxy" });
    expect(within(item).getByText("Key set (never shown)")).toBeInTheDocument();
    expect(within(item).getByText("CLIProxyAPI")).toBeInTheDocument();
    expect(within(item).getByText("http://proxy.local:8317")).toBeInTheDocument();
  });

  it("checks one, replaces and removes its key, and removes it after asking", async () => {
    const { h } = withEndpoints([PROXY]);
    const item = () => within(endpointsRegion()).getByRole("group", { name: "Endpoint CLIProxyAPI" });
    expect(within(item()).getByText("Claude Code · 2 models · checked 1 h ago")).toBeInTheDocument();
    expect(within(item()).getByText("Key set (never shown)")).toBeInTheDocument();

    await userEvent.click(within(item()).getByRole("button", { name: "Check CLIProxyAPI" }));
    expect(sentOf(h, "endpoint.check")).toEqual([{ type: "endpoint.check", endpoint: "cliproxy" }]);
    expect(await within(item()).findByText("42 models · 180 ms")).toBeInTheDocument();

    await userEvent.click(within(item()).getByRole("button", { name: "Replace CLIProxyAPI's key" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "Edit CLIProxyAPI" });
    await userEvent.type(within(form).getByLabelText("Key"), "sk-rotated-1");
    await userEvent.click(within(form).getByRole("button", { name: "Save" }));
    await waitFor(() => expect(sentOf(h, "endpoint.set")).toHaveLength(1));
    expect(sentOf(h, "endpoint.set")[0]).toMatchObject({ endpoint: { id: "cliproxy", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude"] }, key: { type: "set" } });
    expect(JSON.stringify(h.command.mock.calls)).not.toContain("sk-rotated-1");

    await userEvent.click(within(item()).getByRole("button", { name: "Remove CLIProxyAPI's key" }));
    await waitFor(() => expect(sentOf(h, "endpoint.set")[1]?.key).toEqual({ type: "clear" }));
    expect(await within(item()).findByText("No key")).toBeInTheDocument();

    await userEvent.click(within(item()).getByRole("button", { name: "Remove CLIProxyAPI" }));
    expect(sentOf(h, "endpoint.remove")).toEqual([]);
    expect(within(item()).getByText(/Its providers go back to their own login/)).toBeInTheDocument();
    await userEvent.click(within(item()).getByRole("button", { name: "Remove CLIProxyAPI" }));
    await waitFor(() => expect(sentOf(h, "endpoint.remove")).toEqual([{ type: "endpoint.remove", endpoint: "cliproxy" }]));
    expect(await within(endpointsRegion()).findByText(/No endpoints/)).toBeInTheDocument();
  });

  it("switches a provider between its own login and an endpoint from its card, keeping the key", async () => {
    const { h } = withEndpoints([PROXY]);
    const providers = screen.getByRole("region", { name: "Providers" });
    expect(within(providers).getByText("via CLIProxyAPI")).toBeInTheDocument();
    await userEvent.click(within(providers).getByRole("button", { name: "Codex details" }));
    const codex = within(providers).getByRole("combobox", { name: "Codex endpoint" });
    expect(codex).toHaveTextContent("Own login");
    await userEvent.click(codex);
    await userEvent.click(await screen.findByRole("option", { name: /CLIProxyAPI/ }));
    await waitFor(() => expect(sentOf(h, "endpoint.set")).toHaveLength(1));
    expect(sentOf(h, "endpoint.set")[0]).toEqual({ type: "endpoint.set", endpoint: { id: "cliproxy", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"] }, key: { type: "keep" } });

    await userEvent.click(within(providers).getByRole("button", { name: "Claude Code details" }));
    await userEvent.click(within(providers).getByRole("combobox", { name: "Claude Code endpoint" }));
    await userEvent.click(await screen.findByRole("option", { name: "Own login" }));
    await waitFor(() => expect(sentOf(h, "endpoint.set")[1]).toMatchObject({ endpoint: { id: "cliproxy", providers: ["codex"] }, key: { type: "keep" } }));

    // New endpoint… opens the editor with the provider ticked.
    await userEvent.click(within(providers).getByRole("combobox", { name: "Codex endpoint" }));
    await userEvent.click(await screen.findByRole("option", { name: /New endpoint/ }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New endpoint" });
    expect(within(form).getByRole("checkbox", { name: /Codex/ })).toBeChecked();
    expect(within(form).getByRole("checkbox", { name: /Claude Code/ })).not.toBeChecked();
    expect(sentOf(h, "endpoint.set")).toHaveLength(2);
  });

  it("disables a provider that can't speak the kind, says why, and shows what the daemon refuses", async () => {
    const { h } = withEndpoints([], {}, "Codex speaks the OpenAI API: use an endpoint it speaks or CLIProxyAPI");
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New endpoint" });
    await userEvent.click(within(within(form).getByRole("group", { name: "Endpoint kind" })).getByRole("button", { name: "OpenAI" }));
    expect(within(form).getByRole("checkbox", { name: /Claude Code/ })).toBeDisabled();
    expect(within(form).getByText("Claude Code speaks the Anthropic API")).toBeInTheDocument();
    expect(within(form).getByRole("checkbox", { name: /Codex/ })).toBeEnabled();
    // Checked before sending, in the daemon's words.
    await userEvent.type(within(form).getByRole("textbox", { name: "Endpoint name" }), "gw");
    await userEvent.type(within(form).getByRole("textbox", { name: "Base URL" }), "https://user:pw@gw.example.com/v1");
    await userEvent.click(within(form).getByRole("button", { name: "Add endpoint" }));
    expect(within(form).getByRole("alert")).toHaveTextContent("put the key in the key field, not in the URL");
    expect(sentOf(h, "endpoint.set")).toEqual([]);
    await userEvent.clear(within(form).getByRole("textbox", { name: "Base URL" }));
    await userEvent.type(within(form).getByRole("textbox", { name: "Base URL" }), "https://gw.example.com/v1");
    await userEvent.click(within(form).getByRole("button", { name: "Add endpoint" }));
    expect(await within(form).findByText(/Codex speaks the OpenAI API/)).toBeInTheDocument();
  });

  it("takes a 1Password reference instead of a key", async () => {
    const { h } = withEndpoints();
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New endpoint" });
    await userEvent.type(within(form).getByRole("textbox", { name: "Endpoint name" }), "Team");
    await userEvent.type(within(form).getByRole("textbox", { name: "Base URL" }), "https://gw.example.com");
    await userEvent.click(within(form).getByRole("switch", { name: "Key from 1Password" }));
    expect(within(form).queryByLabelText("Key")).toBeNull();
    await userEvent.type(within(form).getByRole("textbox", { name: "1Password reference" }), "op://Work/gw/credential");
    await userEvent.click(within(form).getByRole("button", { name: "Add endpoint" }));
    await waitFor(() =>
      expect(sentOf(h, "endpoint.set")[0]).toEqual({ type: "endpoint.set", endpoint: { id: "team", name: "Team", kind: "cliproxy", base_url: "https://gw.example.com", key_ref: "op://Work/gw/credential" }, key: { type: "keep" } }),
    );
  });

  it("an old daemon's endpoints are read-only, and say to update roll", () => {
    withEndpoints([PROXY], { info: { ...machine().info!, seal_key: undefined, providers: [provider()], settings: machineConfig({}, { endpoints: [PROXY] }) } });
    expect(within(endpointsRegion()).getByText("Update roll on this machine to set endpoints from roll.")).toBeInTheDocument();
    expect(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" })).toBeDisabled();
    expect(within(endpointsRegion()).getByRole("button", { name: "Check CLIProxyAPI" })).toBeDisabled();
    expect(within(endpointsRegion()).getByRole("button", { name: "Remove CLIProxyAPI's key" })).toBeDisabled();
  });

  it("an offline computer's are read-only until it's back; an asleep sandbox's stay editable (it wakes)", async () => {
    const { h } = withEndpoints([PROXY], { online: false });
    expect(within(endpointsRegion()).getByText(/Commands need ada-mbp online/)).toBeInTheDocument();
    expect(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" })).toBeDisabled();
    h.client.enqueue({
      type: "machine",
      cursor: 99,
      machine: { ...h.client.data.machines.get("mch_1")!.value, sandbox: { backend: "docker", size: "standard", status: "stopped", hourly_cents: 3, stopped_reason: "idle" } },
    });
    await waitFor(() => expect(within(endpointsRegion()).getByRole("button", { name: "Add endpoint" })).toBeEnabled());
  });
});

describe("the owner's endpoint profiles", () => {
  const machines = () => [
    machine({ info: { ...machine().info!, seal_key: SEAL_KEY, providers: [provider(), provider({ id: "codex", name: "Codex" })], settings: machineConfig() } }),
    machine({
      id: "mch_2",
      name: "build-box",
      info: { ...machine().info!, hostname: "build-box", seal_key: SEAL_KEY, providers: [provider({ id: "codex", name: "Codex" })], settings: machineConfig({}, { providers: [{ id: "codex", name: "Codex", binary: null, args: 0, env: [] }] }) },
    }),
    machine({ id: "mch_3", name: "old-box", info: { ...machine().info!, hostname: "old-box", settings: machineConfig() } }),
    machine({ id: "mch_4", name: "laptop", online: false, info: { ...machine().info!, hostname: "laptop", seal_key: SEAL_KEY, settings: machineConfig() } }),
  ];

  function profiles(seed: EndpointProfile[] = []) {
    const services = fakeServices({ endpoints: { seed } });
    const h = agentsHarness({ machines: machines(), ctx: { services } });
    renderAgents(<SettingsPanel target="owner" onClose={vi.fn()} />, h);
    return { h, services };
  }

  it("adds, edits and removes profiles, never with a key, and marks one the default for new sandboxes", async () => {
    const { services } = profiles();
    expect(await within(endpointsRegion()).findByText(/No profiles yet/)).toBeInTheDocument();
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Add profile" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New profile" });
    expect(within(form).queryByLabelText("Key")).toBeNull();
    await userEvent.type(within(form).getByRole("textbox", { name: "Endpoint name" }), "CLIProxyAPI");
    await userEvent.type(within(form).getByRole("textbox", { name: "Base URL" }), "http://proxy.local:8317");
    await userEvent.click(within(form).getByRole("checkbox", { name: /Claude Code/ }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /Codex/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Add profile" }));
    await waitFor(() => expect(services.endpoints.save).toHaveBeenCalledWith("acme", { id: "cliproxyapi", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"] }));
    const row = await within(endpointsRegion()).findByRole("group", { name: "Profile CLIProxyAPI" });
    expect(within(row).getByText("Claude Code, Codex · Key typed when applied")).toBeInTheDocument();

    await userEvent.click(within(row).getByRole("switch", { name: "CLIProxyAPI: default for new sandboxes" }));
    await waitFor(() => expect(services.endpoints.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ id: "cliproxyapi", sandbox_default: true })));
    await waitFor(() => expect(within(endpointsRegion()).getByRole("switch", { name: "CLIProxyAPI: default for new sandboxes" })).toBeChecked());

    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Edit CLIProxyAPI" }));
    const edit = within(endpointsRegion()).getByRole("form", { name: "Edit CLIProxyAPI" });
    await userEvent.clear(within(edit).getByRole("textbox", { name: "Endpoint name" }));
    await userEvent.type(within(edit).getByRole("textbox", { name: "Endpoint name" }), "Team proxy");
    await userEvent.click(within(edit).getByRole("button", { name: "Save" }));
    // The same id; still the sandbox default.
    await waitFor(() => expect(services.endpoints.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ id: "cliproxyapi", name: "Team proxy", sandbox_default: true })));
    expect(await within(endpointsRegion()).findByRole("group", { name: "Profile Team proxy" })).toBeInTheDocument();

    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Remove Team proxy" }));
    expect(services.endpoints.remove).not.toHaveBeenCalled();
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Remove Team proxy" }));
    await waitFor(() => expect(services.endpoints.remove).toHaveBeenCalledWith("acme", "cliproxyapi"));
    expect(await within(endpointsRegion()).findByText(/No profiles yet/)).toBeInTheDocument();
  });

  it("applies one to two machines with the key typed once and sealed to each separately", async () => {
    const { h } = profiles([{ id: "cliproxyapi", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"], updated_at: T0 }]);
    const row = await within(endpointsRegion()).findByRole("group", { name: "Profile CLIProxyAPI" });
    await userEvent.click(within(row).getByRole("button", { name: "Apply to machines…" }));
    const dialog = screen.getByRole("dialog", { name: "Apply CLIProxyAPI" });
    const list = within(dialog).getByRole("list", { name: "Machines" });
    expect(within(list).getByRole("checkbox", { name: "ada-mbp" })).toBeChecked();
    expect(within(list).getByRole("checkbox", { name: "build-box" })).toBeChecked();
    expect(within(list).getByRole("checkbox", { name: "old-box" })).toBeDisabled();
    expect(within(list).getByText("Needs a newer roll")).toBeInTheDocument();
    expect(within(list).getByRole("checkbox", { name: "laptop" })).toBeDisabled();
    expect(within(list).getByText("Offline")).toBeInTheDocument();
    const key = within(dialog).getByLabelText("Key");
    expect(key).toHaveAttribute("type", "password");
    expect(key).toHaveAttribute("autocomplete", "off");
    await userEvent.type(key, KEY);
    await userEvent.click(within(dialog).getByRole("button", { name: "Apply to 2 machines" }));
    await waitFor(() => expect(within(dialog).getAllByText("Applied")).toHaveLength(2));

    const sets = h.command.mock.calls.filter((c) => c[2].type === "endpoint.set");
    expect(sets.map((c) => c[0]).sort()).toEqual(["mch_1", "mch_2"]);
    const byMachine = Object.fromEntries(sets.map((c) => [c[0], c[2] as Extract<Command, { type: "endpoint.set" }>]));
    // The same id everywhere, with each machine's own providers.
    expect(byMachine.mch_1.endpoint).toEqual({ id: "cliproxyapi", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"] });
    expect(byMachine.mch_2.endpoint).toEqual({ id: "cliproxyapi", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["codex"] });
    const epks = [byMachine.mch_1.key, byMachine.mch_2.key].map((k) => (k?.type === "set" ? k.sealed.epk : null));
    expect(epks[0]).toBeTruthy();
    expect(epks[1]).toBeTruthy();
    expect(epks[0]).not.toBe(epks[1]);
    expect(JSON.stringify(h.command.mock.calls)).not.toContain(KEY);
    expect(document.body.innerHTML).not.toContain(KEY);
    await userEvent.click(within(dialog).getByRole("button", { name: "Done" }));
    expect(screen.queryByRole("dialog", { name: "Apply CLIProxyAPI" })).toBeNull();
  });
});
