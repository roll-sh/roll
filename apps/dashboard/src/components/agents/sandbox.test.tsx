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
  it("shows its size, status, hourly price and this month's usage", () => {
    const s = sb();
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state(s) } });
    renderAgents(<MachineNode machine={sandboxMachine("running")} />, h);
    const panel = screen.getByRole("group", { name: "Sandbox" });
    expect(within(panel).getByText("$0.03/h")).toBeInTheDocument();
    expect(within(panel).getByText("2 vCPU · 4 GB")).toHaveAttribute("title", "standard: 2 vCPU, 4 GB, 20 GB disk");
    expect(within(panel).getByText("12.5 h")).toBeInTheDocument();
    expect(within(panel).getByText("$0.38")).toBeInTheDocument();
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

  it("asleep is not offline: it says it wakes when needed, has no Stop, and pulses while waking", () => {
    const stopped = (stopped_reason: "idle" | "owner") =>
      sandboxMachine("stopped", { online: false, sandbox: { backend: "docker", size: "standard", status: "stopped", hourly_cents: 3, stopped_reason } });
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state(sb({ status: "stopped", desired: "asleep", stopped_reason: "idle" })) } });
    const { rerender } = renderAgents(<MachineNode machine={stopped("idle")} />, h);
    const card = screen.getByLabelText("Machine box");
    expect(within(card).getByText("asleep")).toBeInTheDocument();
    expect(within(card).queryByText("offline")).not.toBeInTheDocument();
    expect(card.className).not.toContain("opacity-60");
    const panel = within(card).getByRole("group", { name: "Sandbox" });
    expect(within(panel).getByText("Asleep")).toBeInTheDocument();
    expect(within(panel).getByText(/Wakes when an agent needs it/)).toBeInTheDocument();
    expect(within(panel).queryByRole("button", { name: "Stop" })).not.toBeInTheDocument();
    expect(within(panel).getByRole("button", { name: "Start" })).toHaveAttribute("title", "Wake it now");

    // Its owner's stop reads differently.
    h.ctx.sandboxes = state(sb({ status: "stopped", desired: "stopped", stopped_reason: "owner" }));
    rerender(<MachineNode machine={stopped("owner")} />);
    expect(screen.getByText("stopped")).toBeInTheDocument();
    expect(within(screen.getByRole("group", { name: "Sandbox" })).getByText("Stopped")).toBeInTheDocument();
    expect(screen.queryByText(/Wakes when an agent needs it/)).not.toBeInTheDocument();

    // Woken by a command: waking, pulsing, until its daemon is back.
    h.ctx.sandboxes = state(sb({ status: "starting", desired: "running" }));
    rerender(<MachineNode machine={sandboxMachine("starting", { online: false })} />);
    expect(screen.getByText("waking")).toHaveClass("shimmer");
    expect(within(screen.getByRole("group", { name: "Sandbox" })).getByText("Waking")).toHaveClass("shimmer");
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
  });
});

describe("MachineNode", () => {
  it("an offline machine is dimmed and says when it was last seen", () => {
    const m = machine({ online: false, last_seen: Date.now() - 3 * 3_600_000 });
    renderAgents(<MachineNode machine={m} />, agentsHarness({ machines: [m] }));
    const card = screen.getByLabelText("Machine ada-mbp");
    expect(card.className).toContain("opacity-60");
    expect(within(card).getByText(/Last seen 3 h ago/)).toBeInTheDocument();
    expect(within(card).queryByRole("button", { name: /Add a project/ })).not.toBeInTheDocument();
  });

  it("a machine with no projects has one obvious next step", () => {
    const { unmount } = renderAgents(<MachineNode machine={machine()} />, agentsHarness());
    const primaryAdd = () => screen.getAllByRole("button", { name: /Add a project/ }).filter((b) => b.className.includes("bg-fg"));
    expect(primaryAdd()).toHaveLength(1);
    unmount();
    renderAgents(<MachineNode machine={machine()} />, agentsHarness({ entities: [mirrored("project", project(), 1)] }));
    expect(primaryAdd()).toHaveLength(0);
    expect(screen.getByRole("button", { name: "Add a project" })).toBeInTheDocument();
  });
});
