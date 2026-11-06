import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Command, EndpointInfo, EndpointProfile, Machine, MachineSettings, Reply } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { setEndpoint } from "@/lib/agents-endpoints";
import { resetAgentsSurface } from "@/lib/agents-surface";
import { modelPrefs } from "@/lib/agents-models";
import { DEFAULT_OWNER_SETTINGS, parseOwnerSettings } from "@/lib/owner-settings";
import { machine, machineConfig, project, provider, SEAL_KEY } from "@/test/agents";
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

/** Types `text` into `field` as one paste (a key per character is most of a test's time), then Enter. */
async function fill(field: HTMLElement, text: string, enter = false) {
  await userEvent.click(field);
  await userEvent.paste(text);
  if (enter) await userEvent.keyboard("{Enter}");
}

describe("a machine's settings", () => {
  it("sets a provider's binary, arguments, environment and custom models, keeping the rest", async () => {
    const { h } = configured({ branch_prefix: "ada" });
    await userEvent.click(screen.getByRole("button", { name: "Claude Code details" }));
    await fill(screen.getByRole("textbox", { name: "Claude Code binary" }), "/opt/claude/bin/claude", true);
    await waitFor(() => expect(configures(h)).toHaveLength(1));
    expect(configures(h)[0]).toEqual({ branch_prefix: "ada", providers: { claude: { binary: "/opt/claude/bin/claude" } } });

    await fill(screen.getByRole("textbox", { name: "New Claude Code argument" }), "--verbose", true);
    await waitFor(() => expect(configures(h)[1]?.providers?.claude).toEqual({ binary: "/opt/claude/bin/claude", args: ["--verbose"] }));

    // config.toml's variables show by name; their values never come here.
    const env = screen.getByRole("list", { name: "Claude Code environment" });
    expect(within(env).getByText("ANTHROPIC_API_KEY")).toBeInTheDocument();
    expect(within(env).getByText("set in config.toml")).toBeInTheDocument();
    await fill(screen.getByRole("textbox", { name: "Variable name" }), "CLAUDE_CONFIG_DIR");
    await fill(screen.getByRole("textbox", { name: "Variable value" }), "~/.claude-work", true);
    await waitFor(() => expect(configures(h)[2]?.providers?.claude?.env).toEqual({ CLAUDE_CONFIG_DIR: "~/.claude-work" }));

    await fill(screen.getByRole("textbox", { name: "Claude Code model slug" }), "claude-opus-5-5-1m");
    await fill(screen.getByRole("textbox", { name: "Claude Code model name" }), "Opus 1M", true);
    await waitFor(() => expect(configures(h)[3]?.providers?.claude?.models).toEqual([{ id: "claude-opus-5-5-1m", name: "Opus 1M" }]));
    expect(configures(h)[3]).toMatchObject({ branch_prefix: "ada", providers: { claude: { binary: "/opt/claude/bin/claude", args: ["--verbose"] } } });
  });

  it("never sends a credential: the panel says to set it in config.toml on the machine", async () => {
    const { h } = configured();
    await userEvent.click(screen.getByRole("button", { name: "Claude Code details" }));
    await fill(screen.getByRole("textbox", { name: "Variable name" }), "ANTHROPIC_API_KEY");
    await fill(screen.getByRole("textbox", { name: "Variable value" }), "sk-ant-secret", true);
    expect(screen.getByRole("alert")).toHaveTextContent("ANTHROPIC_API_KEY may hold a credential or an address");
    expect(screen.getByRole("alert")).toHaveTextContent("/Users/ada/.local/share/roll/agent/config.toml");
    expect(configures(h)).toEqual([]);
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
    await fill(within(form).getByRole("textbox", { name: "Endpoint name" }), "Team proxy");
    await fill(within(form).getByRole("textbox", { name: "Base URL" }), "http://proxy.local:8317/");
    await userEvent.click(within(form).getByRole("checkbox", { name: /Claude Code/ }));
    const key = within(form).getByLabelText("Key");
    expect(key).toHaveAttribute("type", "password");
    expect(key).toHaveAttribute("autocomplete", "off");
    await fill(key, KEY);
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

  it("adds a profile, never with a key, and marks it the default for new sandboxes", async () => {
    const { services } = profiles();
    expect(await within(endpointsRegion()).findByText(/No profiles yet/)).toBeInTheDocument();
    await userEvent.click(within(endpointsRegion()).getByRole("button", { name: "Add profile" }));
    const form = within(endpointsRegion()).getByRole("form", { name: "New profile" });
    expect(within(form).queryByLabelText("Key")).toBeNull();
    await fill(within(form).getByRole("textbox", { name: "Base URL" }), "http://proxy.local:8317");
    await userEvent.click(within(form).getByRole("checkbox", { name: /Claude Code/ }));
    await userEvent.click(within(form).getByRole("checkbox", { name: /Codex/ }));
    await userEvent.click(within(form).getByRole("button", { name: "Add profile" }));
    await waitFor(() => expect(services.endpoints.save).toHaveBeenCalledWith("acme", { id: "cliproxyapi", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"] }));
    const row = await within(endpointsRegion()).findByRole("group", { name: "Profile CLIProxyAPI" });
    await userEvent.click(within(row).getByRole("switch", { name: "CLIProxyAPI: default for new sandboxes" }));
    await waitFor(() => expect(services.endpoints.save).toHaveBeenLastCalledWith("acme", expect.objectContaining({ id: "cliproxyapi", sandbox_default: true })));
  });
});
