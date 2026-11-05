import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { StrictMode } from "react";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Command, Machine, RegistryAgent, RegistryInstalled, Reply } from "@/lib/agents-api";
import { setActiveClient } from "@/lib/agents-client";
import { machine, machineConfig, provider, T0 } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, type AgentsHarness } from "@/test/agents-render";
import { SettingsPanel } from "./settings-panel";

let conn: { client: AgentsHarness["client"] | null; services: ReturnType<typeof fakeServices> | null; error: string | null } = { client: null, services: null, error: null };
vi.mock("./use-connection", () => ({ useAgentsConnection: () => conn, useDemoParam: () => false }));

afterEach(() => setActiveClient(null));

const REGISTRY: RegistryAgent[] = [
  { id: "goose", name: "goose", version: "1.57.0", description: "A local, extensible, open source AI agent", repository: "https://github.com/block/goose", icon: "https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg", kinds: ["binary"], installable: true, integrity: "sha256" },
  { id: "badhash", name: "Bad Hash", version: "1.0.0", icon: "https://evil.example/x.svg", kinds: ["binary"], installable: true, integrity: "none" },
  { id: "fast-agent", name: "fast-agent", version: "0.10.1", kinds: ["uvx"], installable: false, integrity: "registry" },
  { id: "claude-acp", name: "Claude Agent", version: "0.100.0", kinds: ["npx"], installable: true, integrity: "registry", offered: "acp:claude-acp" },
];

/**
 * A machine whose daemon answers the registry commands as a daemon does: installs reply after
 * `hello` says the provider is offered, and `release()` lets a held install finish.
 */
function daemon(over: { registry?: RegistryInstalled[] | undefined; online?: boolean; strict?: boolean } = { registry: [] }) {
  let installed: RegistryInstalled[] = over.registry ?? [];
  const base = machine();
  const goose = provider({ id: "acp:goose", name: "goose", version: "1.57.0", auth: "unknown", models: [] });
  const info = (): Machine["info"] => ({
    ...base.info!,
    providers: [provider(), ...(installed.length ? [goose] : [])],
    settings: machineConfig({}, {
      registry: "registry" in over && over.registry === undefined ? undefined : installed,
      providers: [{ id: "claude", name: "Claude Code", binary: null, args: 0, env: [] }, ...(installed.length ? [{ id: "acp:goose", name: "goose", binary: "/Users/ada/.local/share/roll/agent/acp/tools/goose/1.57.0/darwin-aarch64/goose", args: 1, env: [] }] : [])],
    }),
  });
  const m: Machine = { ...base, ...(over.online === false ? { online: false } : {}), info: info() };
  let cursor = 10;
  let held: (() => void) | null = null;
  const h = agentsHarness({ machines: [m], ctx: { services: fakeServices() } });
  const publish = () => h.client.enqueue({ type: "machine", cursor: ++cursor, machine: { ...m, info: info() } });
  h.command.mockImplementation(async (_m: string, _id: string, c: Command): Promise<Reply> => {
    if (c.type === "registry.list") return { type: "registry", agents: REGISTRY.map((a) => ({ ...a, installed: installed.find((i) => i.id === a.id)?.version, offered: a.offered ?? installed.find((i) => i.id === a.id)?.provider })), fetched_at: T0 };
    if (c.type === "registry.install") {
      await new Promise<void>((r) => (held = r));
      if (c.agent === "badhash") return { type: "error", code: "provider", message: "installing Bad Hash: the download doesn't match the registry's SHA-256" };
      installed = [{ id: c.agent, version: "1.57.0", provider: `acp:${c.agent}`, kind: "binary", installed_at: T0 }];
      publish();
      return { type: "ok" };
    }
    if (c.type === "registry.remove") {
      installed = installed.filter((i) => i.id !== c.agent);
      publish();
      return { type: "ok" };
    }
    return { type: "ok" };
  });
  const panel = <SettingsPanel target={`m:${m.id}`} onClose={vi.fn()} />;
  renderAgents(over.strict ? <StrictMode>{panel}</StrictMode> : panel, h);
  return { h, release: () => (held as (() => void) | null)?.() };
}

const sent = (h: AgentsHarness, type: Command["type"]) => h.command.mock.calls.map((c) => c[2]).filter((c) => c.type === type);

describe("adding an agent from the ACP registry", () => {
  it("lists the registry, installs an agent and shows it as a provider", async () => {
    const { h, release } = daemon();
    await userEvent.click(screen.getByRole("button", { name: "Add agent" }));
    const goose = await screen.findByRole("button", { name: "Install goose" });
    // What each row offers.
    expect(screen.getByRole("button", { name: "Needs uv fast-agent" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "On this machine Claude Agent" })).toBeDisabled();
    expect(screen.getByRole("link", { name: "About goose" })).toHaveAttribute("href", "https://github.com/block/goose");
    // Icons only from the registry's CDN.
    const imgs = [...document.querySelectorAll("[data-agent] img")].map((i) => i.getAttribute("src"));
    expect(imgs).toEqual(["https://cdn.agentclientprotocol.com/registry/v1/latest/goose.svg"]);

    await userEvent.type(screen.getByRole("searchbox", { name: "Search the ACP registry" }), "goo");
    expect(screen.queryByText("fast-agent")).not.toBeInTheDocument();
    await userEvent.click(goose);
    expect(await screen.findByRole("button", { name: "Installing… goose" })).toBeDisabled();
    expect(sent(h, "registry.install")).toEqual([{ type: "registry.install", agent: "goose" }]);
    release();
    expect(await screen.findByRole("button", { name: "Installed goose" })).toBeDisabled();
    // The daemon's hello: a provider card for it, with Remove in its details.
    const providers = screen.getByRole("region", { name: "Providers" });
    await waitFor(() => expect(within(providers).getByRole("button", { name: "goose details" })).toBeInTheDocument());
  });

  // React's development double mount (Next dev) ran the cleanup in between and every answer was
  // dropped: the popover said "Looking at the registry…" for ever in the demo.
  it("lists the registry under StrictMode too", async () => {
    daemon({ registry: [], strict: true });
    await userEvent.click(screen.getByRole("button", { name: "Add agent" }));
    expect(await screen.findByRole("button", { name: "Install goose" })).toBeEnabled();
  });

  it("says why an install failed, inline", async () => {
    const { release } = daemon();
    await userEvent.click(screen.getByRole("button", { name: "Add agent" }));
    await userEvent.click(await screen.findByRole("button", { name: "Install Bad Hash" }));
    await waitFor(() => expect(screen.getByRole("button", { name: "Installing… Bad Hash" })).toBeInTheDocument());
    release();
    expect(await screen.findByRole("alert")).toHaveTextContent("doesn't match the registry's SHA-256");
    expect(screen.getByRole("button", { name: "Install Bad Hash" })).toBeEnabled();
  });

  it("removes an installed agent from its provider card", async () => {
    const { h } = daemon({ registry: [{ id: "goose", version: "1.57.0", provider: "acp:goose", kind: "binary", installed_at: T0 }] });
    await userEvent.click(screen.getByRole("button", { name: "goose details" }));
    const card = within(document.querySelector<HTMLElement>('[data-setting="p.acp:goose"]')!);
    expect(card.getByText(/Version 1\.57\.0, installed by roll/)).toBeInTheDocument();
    await userEvent.click(card.getByRole("button", { name: "Remove" }));
    await userEvent.click(card.getByRole("button", { name: "Remove goose" }));
    await waitFor(() => expect(sent(h, "registry.remove")).toEqual([{ type: "registry.remove", agent: "goose" }]));
    await waitFor(() => expect(screen.queryByRole("button", { name: "goose details" })).not.toBeInTheDocument());
  });

  it("is read-only for a daemon too old to install, or a machine that's offline", async () => {
    daemon({ registry: undefined });
    expect(screen.getByRole("button", { name: "Add agent" })).toBeDisabled();
    expect(screen.getByRole("button", { name: "Add agent" })).toHaveAttribute("title", expect.stringMatching(/Update roll/));
  });
});
