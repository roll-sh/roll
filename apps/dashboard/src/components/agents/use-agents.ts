"use client";

import { createContext, useContext, useEffect, useRef, useSyncExternalStore } from "react";
import type { MachineId, Thread } from "@/lib/agents-api";
import type { AgentsClient, AgentsSnapshot } from "@/lib/agents-client";
import type { AgentsServices } from "@/lib/agents-services";
import type { SandboxesState } from "./sandbox";
import { useMarkState } from "../mark-state";

/** The client's state, re-rendering on every batch. */
export function useAgentsSnapshot(client: AgentsClient | null): AgentsSnapshot | null {
  return useSyncExternalStore(
    client ? client.subscribe : noopSubscribe,
    client ? client.getSnapshot : nullSnapshot,
    client ? client.getSnapshot : nullSnapshot,
  );
}

const noopSubscribe = () => () => {};
const nullSnapshot = () => null;

/** Keep the client connected while mounted. */
export function useHold(client: AgentsClient | null) {
  useEffect(() => (client ? client.hold() : undefined), [client]);
}

/** What the cards and the conversation need: the client, the owner, and the surface's own actions. */
export interface AgentsCtx {
  client: AgentsClient;
  owner: string;
  demo: boolean;
  /** Show a conversation. */
  openThread: (thread: string) => void;
  /** Back to the canvas. */
  close: () => void;
  /** The new-agent composer, in a project when given. */
  startNew?: (project?: string) => void;
  /** Show the canvas on a node (`m:mch_…`, `x:trm_…`, …); a thread's key opens its conversation. */
  focus: (key: string) => void;
  /** A shared thread's replay (`/agents/s/<id>`): nothing can be sent, only looked at. */
  readOnly?: boolean;
  /** What time it is on the canvas (a replay's clock), for "working for 12 s". Default: now. */
  now?: number;
  /** Sandboxes, repos and work items (unset in a share's replay). */
  services?: AgentsServices;
  /** The owner's sandboxes by machine, and this month's usage (loaded by the canvas). */
  sandboxes?: SandboxesState;
  /** A phone-sized screen: tighter bars, one panel at a time. */
  phone?: boolean;
}

export const AgentsContext = createContext<AgentsCtx | null>(null);

export function useAgents(): AgentsCtx {
  const c = useContext(AgentsContext);
  if (!c) throw new Error("useAgents must be used inside <AgentsContext>");
  return c;
}

/** Send a command for a thread to its machine. */
export function machineFor(client: AgentsClient, thread: Thread | string): MachineId | undefined {
  const id = typeof thread === "string" ? thread : thread.id;
  return client.data.threads.get(id)?.machine;
}

/**
 * Drive the shared logo: it thinks while any agent runs, plays success when a run completes and
 * nothing else is running, and a short error when one fails.
 */
export function useMarkDriver(snap: AgentsSnapshot | null) {
  const { setState, play } = useMarkState();
  const prev = useRef<{ busy: boolean; runs: Map<string, string> } | null>(null);
  const data = snap?.data;

  useEffect(() => {
    if (!data) return;
    let busy = false;
    for (const r of data.threads.values()) {
      const s = r.value.status;
      if ((s === "starting" || s === "running" || s === "waiting") && r.value.archived_at === null) busy = true;
    }
    const runs = new Map<string, string>();
    for (const r of data.runs.values()) runs.set(r.value.id, r.value.status);
    const before = prev.current;
    prev.current = { busy, runs };
    if (!before) {
      if (busy) setState("thinking");
      return;
    }
    let completed = false;
    let failed = false;
    for (const [id, status] of runs) {
      const was = before.runs.get(id);
      if (!was || was === status) continue;
      if (status === "completed" && was !== "completed") completed = true;
      if (status === "failed" && was !== "failed") failed = true;
    }
    if (failed) play([["error", 1400]], busy ? "thinking" : "idle");
    else if (busy && !before.busy) setState("thinking");
    else if (!busy && before.busy) {
      if (completed) play([["success", 1800]]);
      else setState("idle");
    } else if (completed && busy) play([["success", 1400]], "thinking");
  }, [data, setState, play]);

  // Leave the logo at rest when the canvas goes away.
  useEffect(() => () => setState("idle"), [setState]);
}
