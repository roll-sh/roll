"use client";

import { Play, Square, Trash2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Machine } from "@/lib/agents-api";
import type { AgentsData } from "@/lib/agents-store";
import type { AgentsServices } from "@/lib/agents-services";
import { canStart, canStop, formatHourly, type Sandbox, type SandboxList, type SandboxUsageRow } from "@/lib/sandboxes-api";
import { useAgents } from "./use-agents";

/** The owner's sandboxes as the canvas sees them: by machine, with this month's usage. */
export interface SandboxesState {
  /** `null` while loading, or when the caller can't manage sandboxes (agent tokens). */
  list: SandboxList | null;
  error: string | null;
  byMachine: ReadonlyMap<string, Sandbox>;
  usage: ReadonlyMap<string, SandboxUsageRow>;
  /** Load again now (after an action; status changes reload on their own). */
  reload: () => void;
}

/** What the sandbox machines look like on the canvas, as one string: a change reloads the list. */
export function sandboxSignature(data: AgentsData): string {
  const parts: string[] = [];
  for (const r of data.machines.values()) if (r.value.sandbox) parts.push(`${r.value.id}:${r.value.sandbox.status}:${r.value.online}`);
  return parts.sort().join(",");
}

/**
 * Load the owner's sandboxes and this month's usage, and again whenever a sandbox machine changes
 * on the stream (the reconciler sends a `machine` event for each). Usage refreshes every few
 * minutes while the canvas is open.
 */
export function useSandboxes(services: AgentsServices | undefined, owner: string, data: AgentsData): SandboxesState {
  const [list, setList] = useState<SandboxList | null>(null);
  const [usage, setUsage] = useState<SandboxUsageRow[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const sig = sandboxSignature(data);
  const gen = useRef(0);
  useEffect(() => {
    if (!services) return;
    const g = ++gen.current;
    (async () => {
      try {
        const l = await services.sandboxes.list(owner);
        if (g !== gen.current) return;
        setList(l);
        setError(null);
        if (l.sandboxes.length) {
          const u = await services.sandboxes.usage(owner).catch(() => null);
          if (g === gen.current && u) setUsage(u.sandboxes);
        }
      } catch (e) {
        if (g === gen.current) setError(e instanceof Error ? e.message : String(e));
      }
    })();
  }, [services, owner, sig, tick]);
  useEffect(() => {
    const t = setInterval(() => setTick((n) => n + 1), 5 * 60_000);
    return () => clearInterval(t);
  }, []);
  const reload = useCallback(() => setTick((n) => n + 1), []);
  return useMemo(
    () => ({
      list,
      error,
      byMachine: new Map((list?.sandboxes ?? []).map((s) => [s.machine, s])),
      usage: new Map(usage.map((u) => [u.sandbox, u])),
      reload,
    }),
    [list, error, usage, reload],
  );
}

const statusLabel: Record<Sandbox["status"], string> = {
  creating: "Starting up",
  running: "Running",
  stopped: "Stopped",
  failed: "Failed",
  deleting: "Deleting",
};

/** `$3.90`. */
export const dollars = (n: number) => `$${n.toFixed(2)}`;

/** A sandbox machine's own panel: size, status, price, this month's usage, start/stop/delete. */
export function SandboxPanel({ machine }: { machine: Machine }) {
  const { owner, services, sandboxes } = useAgents();
  const sb = sandboxes?.byMachine.get(machine.id);
  const info = machine.sandbox!;
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [confirm, setConfirm] = useState(false);
  const status = info.status;
  const used = sb ? sandboxes?.usage.get(sb.id) : undefined;
  const moving = status === "creating" || status === "deleting";

  const act = async (what: string, fn: () => Promise<unknown>) => {
    setBusy(what);
    setError(null);
    try {
      await fn();
      sandboxes?.reload();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
      setConfirm(false);
    }
  };

  const size = sb?.size;
  const tool = "grid size-6 place-items-center rounded-md hover:bg-panel-2 hover:text-fg disabled:opacity-40";
  return (
    <div className="space-y-1 rounded-lg border border-line bg-bg px-2.5 py-2" data-nodrag role="group" aria-label="Sandbox">
      <div className="flex items-center gap-2 text-[12px]">
        <span className={`size-1.5 shrink-0 rounded-full ${moving ? "bg-accent" : status === "running" ? "bg-ok" : status === "failed" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
        {/* Running is the machine's "online": then the line is what's rented. */}
        <span className={`min-w-0 truncate ${moving || busy ? "shimmer" : ""} ${status === "running" && !busy ? "font-mono text-[11px] text-muted" : ""}`} title={size ? `${size.name}: ${size.cpu} vCPU, ${size.memory_gb} GB, ${size.disk_gb} GB disk` : undefined}>
          {busy === "start"
            ? "Starting…"
            : busy === "stop"
              ? "Stopping…"
              : busy === "delete"
                ? "Deleting…"
                : status === "running"
                  ? size
                    ? `${size.cpu} vCPU · ${size.memory_gb} GB`
                    : info.size
                  : statusLabel[status]}
        </span>
        <span className="ml-auto shrink-0 font-mono text-[11px] text-muted">{formatHourly(size?.hourly_cents ?? info.hourly_cents)}</span>
        {sb && services && status !== "deleting" && !confirm && (
          <span className="-mr-1 flex shrink-0 items-center text-subtle">
            {canStart(sb) && (
              <button disabled={busy !== null} onClick={() => act("start", () => services.sandboxes.start(owner, sb.id))} className={tool} aria-label="Start" title="Start">
                <Play className="size-3" />
              </button>
            )}
            {canStop(sb) && (
              <button disabled={busy !== null} onClick={() => act("stop", () => services.sandboxes.stop(owner, sb.id))} className={tool} aria-label="Stop" title="Stop (a stopped sandbox isn't billed)">
                <Square className="size-3" />
              </button>
            )}
            <button onClick={() => setConfirm(true)} className={tool} aria-label="Delete sandbox" title="Delete the sandbox">
              <Trash2 className="size-3" />
            </button>
          </span>
        )}
      </div>
      {used && (
        <p className="text-[11.5px] text-subtle">
          <span className="font-mono text-muted">{used.running_hours.toFixed(1)} h</span> this month · <span className="font-mono text-muted">{dollars(used.price_usd)}</span>
        </p>
      )}
      {status === "creating" && <p className="text-[11.5px] text-subtle">Booting. It connects on its own in a minute or so.</p>}
      {sb?.error && <p className="text-[11.5px] text-danger">{sb.error}</p>}
      {confirm && sb && services && (
        <div className="flex items-center gap-1 text-[11.5px]">
          <button disabled={busy !== null} onClick={() => act("delete", () => services.sandboxes.remove(owner, sb.id))} className="rounded-md px-1.5 py-0.5 text-danger hover:bg-danger/10">
            Delete it and its files
          </button>
          <button onClick={() => setConfirm(false)} className="p-0.5 text-subtle hover:text-fg" aria-label="Keep the sandbox">
            <X className="size-3.5" />
          </button>
        </div>
      )}
      {error && <p className="text-[11.5px] text-danger">{error}</p>}
    </div>
  );
}
