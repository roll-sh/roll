import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { EndpointProfile, Machine } from "@/lib/agents-api";
import type { Sandbox } from "@/lib/sandboxes-api";
import { AddMachineDialog } from "./add-machine";
import { MachineNode } from "./nodes";
import type { SandboxesState } from "./sandbox";
import { machine, machineConfig, provider, SEAL_KEY, T0 } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, SIZE } from "@/test/agents-render";

const sb = (over: Partial<Sandbox> = {}): Sandbox => ({
  id: "sbx_1",
  name: "box",
  machine: "mch_1",
  backend: "docker",
  size: SIZE,
  status: "running",
  desired: "running",
  stopped_reason: null,
  error: null,
  created_at: T0,
  updated_at: T0,
  last_active_at: T0,
  ...over,
});

function state(s: Sandbox, hours = 12.5): SandboxesState {
  return {
    list: { sandboxes: [s], sizes: [SIZE], max: 5, backend: "docker" },
    error: null,
    byMachine: new Map([[s.machine, s]]),
    usage: new Map([[s.id, { sandbox: s.id, name: s.name, size: "standard", hourly_cents: 3, running_ms: hours * 3_600_000, running_hours: hours, price_usd: (hours * 3) / 100 }]]),
    reload: vi.fn(),
  };
}

const sandboxMachine = (status: Sandbox["status"], over: Partial<Machine> = {}) =>
  machine({
    name: "box",
    info: { hostname: "box", os: "linux", arch: "x86_64", version: "0.1.0", home: "/home/roll", providers: [provider(), provider({ id: "codex", name: "Codex" })], settings: null },
    sandbox: { backend: "docker", size: "standard", status, hourly_cents: 3 },
    ...over,
  });

describe("a sandbox machine on the canvas", () => {
  it("stops and starts, and deletes only after asking", async () => {
    const s = sb();
    const stop = vi.fn(async () => sb({ status: "stopped", desired: "stopped" }));
    const remove = vi.fn(async () => null);
    const services = fakeServices({ sandboxes: { stop, remove } });
    const st = state(s);
    const h = agentsHarness({ ctx: { services, sandboxes: st } });
    const { rerender } = renderAgents(<MachineNode machine={sandboxMachine("running")} />, h);
    await userEvent.click(screen.getByRole("button", { name: "Stop" }));
    expect(stop).toHaveBeenCalledWith("acme", "sbx_1");
    await waitFor(() => expect(st.reload).toHaveBeenCalled());

    // Stopped: start it again.
    const start = vi.fn(async () => sb());
    h.ctx.services = fakeServices({ sandboxes: { start, remove } });
    h.ctx.sandboxes = state(sb({ status: "stopped", desired: "stopped" }));
    rerender(<MachineNode machine={sandboxMachine("stopped", { online: false, last_seen: Date.now() - 3 * 3_600_000 })} />);
    await userEvent.click(screen.getByRole("button", { name: "Start" }));
    expect(start).toHaveBeenCalledWith("acme", "sbx_1");

    await userEvent.click(screen.getByRole("button", { name: "Delete sandbox" }));
    expect(remove).not.toHaveBeenCalled();
    await userEvent.click(screen.getByRole("button", { name: "Keep the sandbox" }));
    await userEvent.click(screen.getByRole("button", { name: "Delete sandbox" }));
    await userEvent.click(screen.getByRole("button", { name: "Delete it and its files" }));
    expect(remove).toHaveBeenCalledWith("acme", "sbx_1");
  });

  it("puts logging a provider in first, in a terminal running the device login, and shows that terminal", async () => {
    const m = sandboxMachine("running", {
      info: {
        hostname: "box",
        os: "linux",
        arch: "x86_64",
        version: "0.1.0",
        home: "/home/roll",
        providers: [provider({ auth: "missing" }), provider({ id: "codex", name: "Codex", auth: "missing" })],
        settings: null,
      },
    });
    const h = agentsHarness({ machines: [m], reply: () => ({ type: "created", id: "trm_9" }), ctx: { services: fakeServices(), sandboxes: state(sb()) } });
    renderAgents(<MachineNode machine={m} />, h);
    const login = screen.getByRole("group", { name: "Log in to a provider" });
    await userEvent.click(within(login).getByRole("button", { name: "Log in to Codex" }));
    expect(h.command).toHaveBeenCalledWith("mch_1", expect.any(String), { type: "terminal.open", command: "codex login --device-auth", cols: 100, rows: 26 });
    await waitFor(() => expect(h.focus).toHaveBeenCalledWith("x:trm_9"));
    await userEvent.click(within(login).getByRole("button", { name: "Log in to Claude Code" }));
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), expect.objectContaining({ command: "claude /login" }));
    // Logged in somewhere the daemon didn't see (the daemon checks again when a terminal exits).
    await userEvent.click(within(login).getByRole("button", { name: /check again/i }));
    expect(h.command).toHaveBeenLastCalledWith("mch_1", expect.any(String), { type: "provider.refresh" });
  });
});

describe("the new-sandbox dialog's endpoint", () => {
  const KEY = "sk-sandbox-never-shown-71";
  const PROFILE: EndpointProfile = { id: "cliproxy", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex", "acp:gemini"], sandbox_default: true, updated_at: T0 };
  const booting = machine({ id: "mch_new", name: "scratch", online: false, info: null, sandbox: { backend: "docker", size: "standard", status: "creating", hourly_cents: 3 } });
  const up = machine({
    ...booting,
    online: true,
    sandbox: { backend: "docker", size: "standard", status: "running", hourly_cents: 3 },
    info: { ...machine().info!, hostname: "scratch", seal_key: SEAL_KEY, providers: [provider(), provider({ id: "codex", name: "Codex" })], settings: machineConfig() },
  });

  function rent(seed: EndpointProfile[] = [PROFILE]) {
    const create = vi.fn(async () => ({ sandbox: sb({ machine: "mch_new", name: "scratch", status: "creating" }), machine: booting }));
    const services = fakeServices({ sandboxes: { create }, endpoints: { seed } });
    const h = agentsHarness({ machines: [machine()], ctx: { services, sandboxes: { ...state(sb()), list: { sandboxes: [], sizes: [SIZE], max: 5, backend: "docker" } } } });
    const onCreated = vi.fn();
    const view = renderAgents(<AddMachineDialog onClose={vi.fn()} onCreated={onCreated} />, h);
    return { h, onCreated, create, view };
  }
  const sets = (h: ReturnType<typeof agentsHarness>) => h.command.mock.calls.filter((c) => c[2].type === "endpoint.set");

  it("picks the sandbox default, then applies it with the sealed key once the machine's seal key arrives", async () => {
    const { h, onCreated, create } = rent();
    const rentBox = screen.getByRole("region", { name: "Rent a sandbox" });
    expect(await within(rentBox).findByRole("combobox", { name: "Endpoint" })).toHaveTextContent("CLIProxyAPI");
    const key = within(rentBox).getByLabelText("Endpoint key");
    expect(key).toHaveAttribute("type", "password");
    expect(key).toHaveAttribute("autocomplete", "off");
    await userEvent.type(key, KEY);
    await userEvent.click(within(rentBox).getByRole("button", { name: /^Rent for/ }));
    await waitFor(() => expect(create).toHaveBeenCalled());
    // The sandbox env never carries the endpoint's key.
    expect(JSON.stringify(create.mock.calls)).not.toContain(KEY);

    // Waiting for its daemon: progress, nothing sent, the dialog stays.
    expect(await screen.findByText("Setting up CLIProxyAPI…")).toHaveClass("shimmer");
    expect(sets(h)).toEqual([]);
    expect(onCreated).not.toHaveBeenCalled();
    expect(document.body.innerHTML).not.toContain(KEY);

    // Its hello brings the seal key: applied with the key sealed to it, then the dialog is done.
    h.client.enqueue({ type: "machine", cursor: 5, machine: up });
    await waitFor(() => expect(sets(h)).toHaveLength(1));
    const [[to, , command]] = sets(h);
    expect(to).toBe("mch_new");
    expect(command).toEqual({
      type: "endpoint.set",
      endpoint: { id: "cliproxy", name: "CLIProxyAPI", kind: "cliproxy", base_url: "http://proxy.local:8317", providers: ["claude", "codex"] },
      key: { type: "set", sealed: expect.objectContaining({ v: 1 }) },
    });
    expect(JSON.stringify(h.command.mock.calls)).not.toContain(KEY);
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith(up));
  });
});
