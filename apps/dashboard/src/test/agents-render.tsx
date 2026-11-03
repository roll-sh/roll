/** Render agents canvas components with a real `AgentsClient` over a fake transport. */
import { render } from "@testing-library/react";
import type { ReactElement } from "react";
import { vi, type Mock } from "vitest";
import type { Command, Machine, MachineId, Mirrored, Reply } from "@/lib/agents-api";
import { AgentsClient, type AgentsTransport } from "@/lib/agents-client";
import { AgentsContext, type AgentsCtx } from "@/components/agents/use-agents";
import { machine as machineFixture, M1 } from "./agents";

export interface AgentsHarness {
  client: AgentsClient;
  /** Every command sent, with its machine. */
  command: Mock<(machine: MachineId, id: string, command: Command) => Promise<Reply>>;
  ctx: AgentsCtx;
  setExpanded: Mock<(thread: string, open: boolean) => void>;
  focus: Mock<(key: string) => void>;
}

export function agentsHarness(opts: { machines?: Machine[]; entities?: Mirrored[]; follow?: string[]; reply?: (c: Command) => Reply } = {}): AgentsHarness {
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
  const setExpanded = vi.fn();
  const focus = vi.fn();
  const ctx: AgentsCtx = { client, owner: "acme", demo: false, expanded: new Set(), setExpanded, focus };
  return { client, command, ctx, setExpanded, focus };
}

export function renderAgents(ui: ReactElement, h: AgentsHarness) {
  return render(ui, { wrapper: ({ children }) => <AgentsContext.Provider value={h.ctx}>{children}</AgentsContext.Provider> });
}

export { M1 };
