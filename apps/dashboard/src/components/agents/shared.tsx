"use client";

import { Bot, Cloud, Laptop, Server } from "lucide-react";
import type { DiffStat, Machine, ProviderInfo, RunStatus, ThreadStatus, Usage } from "@/lib/agents-api";

/** 12.3k, 1.2M. */
export function compact(n: number) {
  if (n < 1000) return String(n);
  if (n < 1_000_000) return `${(n / 1000).toFixed(n < 10_000 ? 1 : 0)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

export function usageLine(u: Usage): string | null {
  const tokens = u.input_tokens + u.output_tokens;
  if (!tokens && u.cost_usd == null) return null;
  const cost = u.cost_usd != null ? ` · $${u.cost_usd < 0.01 ? u.cost_usd.toFixed(3) : u.cost_usd.toFixed(2)}` : "";
  return `${compact(tokens)} tok${cost}`;
}

export function Stat({ stat, className = "" }: { stat: DiffStat | null | undefined; className?: string }) {
  if (!stat || (!stat.additions && !stat.deletions)) return null;
  return (
    <span className={`font-mono tabular-nums ${className}`}>
      <span className="text-ok">+{stat.additions}</span> <span className="text-danger">−{stat.deletions}</span>
    </span>
  );
}

export const threadStatusLabel: Record<ThreadStatus, string> = {
  idle: "Idle",
  starting: "Starting",
  running: "Working",
  waiting: "Needs you",
  error: "Error",
};

export const runStatusLabel: Record<RunStatus, string> = {
  queued: "Queued",
  starting: "Starting",
  running: "Working",
  waiting: "Waiting",
  completed: "Done",
  interrupted: "Stopped",
  failed: "Failed",
  cancelled: "Cancelled",
  rolled_back: "Rolled back",
};

/** A small status dot: accent while busy (the canvas pulses the ring), red on error, quiet otherwise. */
export function StatusDot({ status, className = "" }: { status: ThreadStatus | "online" | "offline"; className?: string }) {
  const cls =
    status === "running" || status === "starting" || status === "waiting"
      ? "bg-accent"
      : status === "error"
        ? "bg-danger"
        : status === "online"
          ? "bg-ok"
          : status === "offline"
            ? "bg-subtle/60"
            : "bg-subtle";
  return <span aria-hidden className={`inline-block size-1.5 shrink-0 rounded-full ${cls} ${className}`} />;
}

export function MachineIcon({ machine, className = "size-4" }: { machine: Machine; className?: string }) {
  if (machine.sandbox) return <Cloud className={className} />;
  const os = machine.info?.os ?? "";
  return os === "macos" || os === "windows" ? <Laptop className={className} /> : <Server className={className} />;
}

export function providerName(providers: readonly ProviderInfo[], id: string) {
  return providers.find((p) => p.id === id)?.name ?? (id.startsWith("acp:") ? id.slice(4) : id);
}

export function modelName(providers: readonly ProviderInfo[], provider: string, model: string | null) {
  if (!model) return null;
  return providers.find((p) => p.id === provider)?.models.find((m) => m.id === model)?.name ?? model;
}

export function ProviderGlyph({ className = "size-3.5" }: { className?: string }) {
  return <Bot className={className} />;
}

/** The command that logs a provider in, run in a terminal on the machine. */
export function loginCommand(p: ProviderInfo): string {
  if (p.id === "claude") return "claude /login";
  if (p.id === "codex") return "codex login";
  if (p.id.startsWith("acp:")) return `${p.id.slice(4)} auth login`;
  return `${p.id} login`;
}

export function shortPath(path: string, home?: string | null) {
  if (home && path.startsWith(home)) return `~${path.slice(home.length)}`;
  return path;
}

export function elapsed(ms: number) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ${s % 60}s`;
  return `${Math.floor(m / 60)}h ${m % 60}m`;
}
