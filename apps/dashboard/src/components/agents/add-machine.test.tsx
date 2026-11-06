import { fireEvent, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it, vi } from "vitest";
import type { Machine } from "@/lib/agents-api";
import type { Sandbox } from "@/lib/sandboxes-api";
import { AddMachineDialog, AddMachinePaths, sandboxEnv } from "./add-machine";
import { useSandboxes, type SandboxesState } from "./sandbox";
import { ApiError } from "@/lib/code-api";
import { machine } from "@/test/agents";
import { agentsHarness, fakeServices, renderAgents, SIZE } from "@/test/agents-render";

const LARGE = { name: "large", cpu: 4, memory_gb: 8, disk_gb: 40, hourly_cents: 6 };

function state(over: Partial<SandboxesState> = {}): SandboxesState {
  return {
    list: { sandboxes: [], sizes: [SIZE, LARGE], max: 3, backend: "docker" },
    error: null,
    byMachine: new Map(),
    usage: new Map(),
    reload: vi.fn(),
    ...over,
  };
}

const booting: Machine = machine({ id: "mch_new", name: "scratch", online: false, info: null, sandbox: { backend: "docker", size: "large", status: "creating", hourly_cents: 6 } });
const sandbox = { id: "sbx_1", name: "scratch", machine: "mch_new", backend: "docker", size: LARGE, status: "creating", desired: "running", stopped_reason: null, error: null, created_at: 0, updated_at: 0, last_active_at: 0 } satisfies Sandbox;

describe("AddMachinePaths", () => {
  it("shows both ways in: the install steps and the sizes with their hourly price", () => {
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state() } });
    renderAgents(<AddMachinePaths />, h);
    const computer = screen.getByRole("region", { name: "Connect your computer" });
    expect(within(computer).getByText(/install\.sh \| sh$/)).toBeInTheDocument();
    expect(within(computer).getByText("roll agent up")).toBeInTheDocument();
    const rent = screen.getByRole("region", { name: "Rent a sandbox" });
    expect(within(rent).getByRole("radio", { name: /standard.*\$0\.03\/h/ })).toBeChecked();
    expect(within(rent).getByRole("radio", { name: /large.*4 vCPU.*\$0\.06\/h/ })).toBeInTheDocument();
    expect(within(rent).getByRole("button", { name: "Rent for $0.03/h" })).toBeEnabled();
    expect(within(rent).getByText("0 of 3 in use")).toBeInTheDocument();
    // The whole spec, on its own line under the name (one line cut the disk off).
    const spec = within(rent).getByText(/4 vCPU · 8 GB · 40 GB disk/);
    expect(spec.className).not.toContain("truncate");
    // The two ways in are as tall as what they hold, not stretched to each other.
    expect(computer.parentElement!.className).toContain("items-start");
  });

  it("asks for keys in the UI's type; only a typed key is mono", async () => {
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state() } });
    renderAgents(<AddMachinePaths />, h);
    await userEvent.click(screen.getByRole("button", { name: /Add provider API keys/ }));
    for (const key of screen.getAllByPlaceholderText(/API key/)) expect(key.className).toMatch(/placeholder-shown:font-sans.*|font-mono.*placeholder-shown:font-sans/);
  });

  it("rents the chosen size with only the keys given, and the machine is on the canvas at once", async () => {
    const create = vi.fn(async () => ({ sandbox, machine: booting }));
    const services = fakeServices({ sandboxes: { create } });
    const st = state();
    const h = agentsHarness({ machines: [], ctx: { services, sandboxes: st } });
    const onCreated = vi.fn();
    renderAgents(<AddMachinePaths onCreated={onCreated} />, h);
    await userEvent.click(screen.getByRole("radio", { name: /large/ }));
    await userEvent.type(screen.getByRole("textbox", { name: "Sandbox name" }), "scratch");
    await userEvent.click(screen.getByRole("button", { name: /Add provider API keys/ }));
    await userEvent.type(screen.getByLabelText("Anthropic API key"), " sk-ant-test ");
    await userEvent.click(screen.getByRole("button", { name: "Rent for $0.06/h" }));
    await waitFor(() => expect(onCreated).toHaveBeenCalledWith(booting));
    expect(create).toHaveBeenCalledWith("acme", { name: "scratch", size: "large", env: { ANTHROPIC_API_KEY: "sk-ant-test" } });
    expect(h.client.data.machines.get("mch_new")?.value.sandbox?.status).toBe("creating");
    expect(st.reload).toHaveBeenCalled();
  });

  it("says so when sandboxes are off, the caller can't rent, or the owner is at the limit", () => {
    const h = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state({ list: { sandboxes: [], sizes: [SIZE], max: 3, backend: null } }) } });
    const { unmount } = renderAgents(<AddMachinePaths />, h);
    expect(screen.getByText("Sandboxes aren't on for this roll server.")).toBeInTheDocument();
    unmount();

    const agent = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state({ list: null, error: { message: "Only the owner can rent sandboxes.", forbidden: true } }) } });
    const second = renderAgents(<AddMachinePaths />, agent);
    expect(screen.getByText("Only the owner can rent sandboxes.")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: "Try again" })).toBeNull();
    second.unmount();

    // The server failing isn't "only the owner" (a 500 said so): what went wrong, and Try again.
    const broken = state({ list: null, error: { message: "Couldn't load sandboxes: internal error", forbidden: false } });
    const third = renderAgents(<AddMachinePaths />, agentsHarness({ ctx: { services: fakeServices(), sandboxes: broken } }));
    expect(screen.getByRole("alert")).toHaveTextContent("Couldn't load sandboxes: internal error");
    fireEvent.click(screen.getByRole("button", { name: "Try again" }));
    expect(broken.reload).toHaveBeenCalled();
    third.unmount();

    const full = agentsHarness({ ctx: { services: fakeServices(), sandboxes: state({ list: { sandboxes: [sandbox, sandbox, sandbox], sizes: [SIZE], max: 3, backend: "docker" } }) } });
    renderAgents(<AddMachinePaths />, full);
    expect(screen.getByRole("button", { name: "Rent for $0.03/h" })).toBeDisabled();
    expect(screen.getByText(/Delete one to rent another/)).toBeInTheDocument();
  });

  it("keeps a refused create on screen as an error", async () => {
    const services = fakeServices({ sandboxes: { create: vi.fn(async () => Promise.reject(new Error("sandboxes need ROLL_SECRET_KEY on the server"))) } });
    renderAgents(<AddMachinePaths />, agentsHarness({ ctx: { services, sandboxes: state() } }));
    await userEvent.click(screen.getByRole("button", { name: "Rent for $0.03/h" }));
    expect(await screen.findByText("sandboxes need ROLL_SECRET_KEY on the server")).toBeInTheDocument();
  });

  it("the dialog closes on Escape", async () => {
    const onClose = vi.fn();
    renderAgents(<AddMachineDialog onClose={onClose} onCreated={vi.fn()} />, agentsHarness({ ctx: { services: fakeServices(), sandboxes: state() } }));
    expect(screen.getByRole("dialog", { name: "Add a machine" })).toBeInTheDocument();
    await userEvent.keyboard("{Escape}");
    expect(onClose).toHaveBeenCalled();
  });

  it("sends only keys that have a value", () => {
    expect(sandboxEnv({ ANTHROPIC_API_KEY: "  ", OPENAI_API_KEY: " sk " })).toEqual({ OPENAI_API_KEY: "sk" });
    expect(sandboxEnv({})).toBeUndefined();
  });
});

describe("loading the owner's sandboxes", () => {
  it("tells a server error (try again) from not being the owner", async () => {
    const data = agentsHarness().client.data;
    for (const [status, message, forbidden] of [
      [500, "Couldn't load sandboxes: internal error", false],
      [403, "Only the owner can rent sandboxes.", true],
    ] as const) {
      const services = fakeServices({ sandboxes: { list: vi.fn().mockRejectedValue(new ApiError("internal error", status, null)) } });
      const { result, unmount } = renderHook(() => useSandboxes(services, "acme", data));
      await waitFor(() => expect(result.current.error).toEqual({ message, forbidden }));
      unmount();
    }
  });
});
