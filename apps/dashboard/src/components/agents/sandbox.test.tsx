import { screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Machine } from "@/lib/agents-api";
import type { Sandbox } from "@/lib/sandboxes-api";
import { MachineNode } from "./nodes";
import type { SandboxesState } from "./sandbox";
import { machine, mirrored, project, provider, T0 } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, SIZE } from "@/test/agents-render";

const sb = (over: Partial<Sandbox> = {}): Sandbox => ({
  id: "sbx_1",
  name: "box",
  machine: "mch_1",
  backend: "docker",
  size: SIZE,
  status: "running",
  desired: "running",
  error: null,
  created_at: T0,
  updated_at: T0,
  ...over,
});

function state(s: Sandbox, hours = 12.5): SandboxesState {
  return {
    list: { sandboxes: [s], sizes: [SIZE], max: 5, backend: "docker" },
    error: null,
    byMachine: new Map([[s.machine, s]]),
    usage: new Map([[s.id, { sandbox: s.id, name: s.name, size: "standard", monthly_cents: 2000, running_ms: hours * 3_600_000, running_hours: hours, price_usd: (hours * 20) / 730 }]]),
    reload: vi.fn(),
  };
}

const sandboxMachine = (status: Sandbox["status"], over: Partial<Machine> = {}) =>
  machine({
    name: "box",
    info: { hostname: "box", os: "linux", arch: "x86_64", version: "0.1.0", home: "/home/roll", providers: [provider(), provider({ id: "codex", name: "Codex" })] },
    sandbox: { backend: "docker", size: "standard", status, monthly_cents: 2000 },
    ...over,
  });

describe("a sandbox machine on the canvas", () => {
  it("shows its size, status, monthly price and this month's usage", () => {
    const s = sb();
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state(s) } });
    renderAgents(<MachineNode machine={sandboxMachine("running")} />, h);
    const panel = screen.getByRole("group", { name: "Sandbox" });
    expect(within(panel).getByText("Running")).toBeInTheDocument();
    expect(within(panel).getByText("$20/mo")).toBeInTheDocument();
    expect(within(panel).getByText("standard · 2 vCPU · 4 GB · 20 GB disk")).toBeInTheDocument();
    expect(within(panel).getByText("12.5 h")).toBeInTheDocument();
    expect(within(panel).getByText("$0.34")).toBeInTheDocument();
  });

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

  it("is 'booting' while it's created and 'connecting' until its daemon is up", () => {
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state(sb({ status: "creating" })) } });
    const { rerender } = renderAgents(<MachineNode machine={sandboxMachine("creating", { online: false, info: null })} />, h);
    expect(screen.getByText("booting")).toBeInTheDocument();
    expect(screen.getByText(/connects on its own/)).toBeInTheDocument();
    rerender(<MachineNode machine={sandboxMachine("running", { online: false, info: null })} />);
    expect(screen.getByText("connecting")).toBeInTheDocument();
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
  });
});

describe("MachineNode", () => {
  it("an offline machine is dimmed and says when it was last seen", () => {
    const m = machine({ online: false, last_seen: Date.now() - 3 * 3_600_000 });
    renderAgents(<MachineNode machine={m} />, agentsHarness({ machines: [m] }));
    const card = screen.getByLabelText("Machine ada-mbp");
    expect(card.className).toContain("opacity-60");
    expect(within(card).getByText(/Last seen 3 h ago/)).toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: /Add project/ })).not.toBeInTheDocument();
  });

  it("a machine with no projects has one obvious next step", () => {
    const { unmount } = renderAgents(<MachineNode machine={machine()} />, agentsHarness());
    expect(screen.getByText(/Add a project: a folder here/)).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add project" }).className).toContain("bg-fg");
    unmount();
    renderAgents(<MachineNode machine={machine()} />, agentsHarness({ entities: [mirrored("project", project(), 1)] }));
    expect(screen.queryByText(/Add a project: a folder here/)).not.toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Add project" }).className).not.toContain("bg-fg");
  });
});
