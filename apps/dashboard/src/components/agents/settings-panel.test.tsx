import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command, Machine, MachineSettings, Reply } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { agentsSurface, resetAgentsSurface, viewFromHash, hashFor } from "@/lib/agents-surface";
import { machine, machineConfig, provider } from "@/test/agents";
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
  const codex = provider({ id: "codex", name: "Codex", auth: "missing", models: [{ id: "gpt-5.5-codex", name: "GPT-5.5 Codex", default: true, efforts: [] }] });
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

  it("says why a save failed", async () => {
    const services = fakeServices({ settings: { save: vi.fn(async () => Promise.reject(new Error("roll is down"))) } });
    owner(services);
    await userEvent.keyboard("{Meta>},{/Meta}");
    await userEvent.click(await screen.findByRole("switch", { name: "Confirm before deleting a thread" }));
    expect(await screen.findByRole("alert")).toHaveTextContent("roll is down");
  });
});

describe("what the settings change", () => {
  it("the send key: with ⌘/Ctrl+Enter, Enter breaks the line", async () => {
    const onSubmit = vi.fn(async () => {});
    const h = agentsHarness({ ctx: { prefs: { settings: { notifications: "off", send: "mod_enter", follow_up: "queue", confirm_delete: true, defaults: {} }, revision: 1, loaded: true, error: null, save: vi.fn() } } });
    renderAgents(<Composer mode={{ kind: "new", project: { ...projectFixture(), settings: { auto_pull: false, defaults: {}, scripts: [] } }, machine: machine() }} onSubmit={onSubmit} />, h);
    const box = screen.getByRole("textbox", { name: "Message" });
    await userEvent.type(box, "line one{Enter}line two");
    expect(onSubmit).not.toHaveBeenCalled();
    expect(box).toHaveValue("line one\nline two");
    await userEvent.keyboard("{Control>}{Enter}{/Control}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ prompt: "line one\nline two" })));
  });

  it("new threads start from the machine's defaults, then the owner's, after the project's", async () => {
    const onSubmit = vi.fn(async () => {});
    const m = machine({
      info: { ...machine().info!, providers: [provider(), provider({ id: "codex", name: "Codex", models: [] })], settings: machineConfig({ defaults: { runtime_mode: "ask" } }) },
    });
    const prefs = { settings: { notifications: "off" as const, send: "enter" as const, follow_up: "queue" as const, confirm_delete: true, defaults: { provider: "codex", interaction_mode: "plan" as const } }, revision: 1, loaded: true, error: null, save: vi.fn() };
    renderAgents(<Composer mode={{ kind: "new", project: projectFixture(), machine: m }} onSubmit={onSubmit} />, agentsHarness({ ctx: { prefs } }));
    expect(screen.getByRole("button", { name: /^Model: Codex/ })).toBeInTheDocument();
    await userEvent.type(screen.getByRole("textbox", { name: "Message" }), "go{Enter}");
    await waitFor(() => expect(onSubmit).toHaveBeenCalledWith(expect.objectContaining({ provider: "codex", runtime_mode: "ask", interaction_mode: "plan" })));
  });
});

function projectFixture() {
  return { id: "prj_1", name: "roll", path: "/Users/ada/src/roll", repo: null, git: null, settings: { auto_pull: false, defaults: {}, scripts: [] }, pull: null, created_at: 1, updated_at: 1 };
}
