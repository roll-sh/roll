/** Render agents canvas components with a real `AgentsClient` over a fake transport. */
import { render } from "@testing-library/react";
import type { ReactElement } from "react";
import { vi, type Mock } from "vitest";
import type { Command, Machine, MachineId, Mirrored, Reply } from "@/lib/agents-api";
import { AgentsClient, type AgentsTransport } from "@/lib/agents-client";
import { memoryCanvas, memorySettings, type AgentsServices } from "@/lib/agents-services";
import { NO_INTEGRATIONS } from "@/lib/integrations-api";
import { DISCONNECTED, memoryLinear } from "@/lib/linear-api";
import type { SandboxSize } from "@/lib/sandboxes-api";
import { AgentsContext, type AgentsCtx } from "@/components/agents/use-agents";
import { machine as machineFixture, M1 } from "./agents";

export interface AgentsHarness {
  client: AgentsClient;
  /** Every command sent, with its machine. */
  command: Mock<(machine: MachineId, id: string, command: Command) => Promise<Reply>>;
  ctx: AgentsCtx;
  openThread: Mock<(thread: string) => void>;
  close: Mock<() => void>;
  focus: Mock<(key: string) => void>;
}

export function agentsHarness(
  opts: { machines?: Machine[]; entities?: Mirrored[]; follow?: string[]; reply?: (c: Command) => Reply; ctx?: Partial<AgentsCtx> } = {},
): AgentsHarness {
  const command = vi.fn(async (_m: MachineId, _id: string, c: Command): Promise<Reply> => opts.reply?.(c) ?? { type: "ok" });
  const transport: AgentsTransport = {
    state: async () => ({ cursor: 0, machines: [], entities: [] }),
    stream: () => () => {},
    command,
  };
  const client = new AgentsClient("acme", transport, (fn) => fn());
  // Load the records through the same path the stream uses.
  client.follow("test", opts.follow ?? []);
  for (const m of opts.machines ?? [machineFixture()]) client.enqueue({ type: "machine", cursor: 1, machine: m });
  for (const e of opts.entities ?? []) client.enqueue({ type: "entity", ...e });
  const openThread = vi.fn();
  const close = vi.fn();
  const focus = vi.fn();
  const ctx: AgentsCtx = { client, owner: "acme", demo: false, openThread, close, focus, ...opts.ctx };
  return { client, command, ctx, openThread, close, focus };
}

export function renderAgents(ui: ReactElement, h: AgentsHarness) {
  return render(ui, { wrapper: ({ children }) => <AgentsContext.Provider value={h.ctx}>{children}</AgentsContext.Provider> });
}

/**
 * Services with every call a mock; `over` replaces some. Sandboxes: one size, none rented, docker.
 * The canvas and the owner's settings are kept in memory (revisions and conflicts as the relay does
 * them), behind mocks.
 */
export function fakeServices(
  over: Omit<Partial<AgentsServices>, "sandboxes" | "canvas" | "settings" | "machines" | "integrations"> & {
    sandboxes?: Partial<AgentsServices["sandboxes"]>;
    canvas?: Partial<AgentsServices["canvas"]>;
    settings?: Partial<AgentsServices["settings"]>;
    machines?: Partial<AgentsServices["machines"]>;
    integrations?: Partial<AgentsServices["integrations"]>;
  } = {},
): AgentsServices {
  const { sandboxes, canvas, settings, machines, integrations, ...rest } = over;
  const kept = memoryCanvas();
  const prefs = memorySettings();
  return {
    sandboxes: {
      list: vi.fn(async () => ({ sandboxes: [], sizes: [SIZE], max: 5, backend: "docker" })),
      create: vi.fn(),
      start: vi.fn(),
      stop: vi.fn(),
      remove: vi.fn(async () => null),
      usage: vi.fn(async () => ({ owner: "acme", month: "2026-11", sandboxes: [], totals: { running_ms: 0, running_hours: 0, price_usd: 0 } })),
      ...sandboxes,
    },
    repos: vi.fn(async () => []),
    createProject: vi.fn(),
    takeWork: vi.fn(async () => null),
    releaseWork: vi.fn(async () => {}),
    machines: {
      rename: vi.fn(async (_o: string, id: MachineId, name: string) => ({ ...machineFixture(), id, name })),
      remove: vi.fn(async () => {}),
      ...machines,
    },
    canvas: { load: vi.fn(kept.load), save: vi.fn(kept.save), ...canvas },
    settings: { load: vi.fn(prefs.load), save: vi.fn(prefs.save), ...settings },
    integrations: {
      list: vi.fn(async () => NO_INTEGRATIONS),
      install: vi.fn(),
      update: vi.fn(),
      remove: vi.fn(async () => {}),
      ...integrations,
    },
    linear: memoryLinear(DISCONNECTED),
    ...rest,
  };
}

export const SIZE: SandboxSize = { name: "standard", cpu: 2, memory_gb: 4, disk_gb: 20, hourly_cents: 3 };

export { M1 };
