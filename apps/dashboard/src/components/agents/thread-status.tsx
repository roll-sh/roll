"use client";

/**
 * A thread's status as T3 Code shows it on its rows (T3's `Sidebar.logic.ts`
 * `resolveThreadStatusPill` and `ThreadStatusIndicators.tsx`, MIT). In roll's colours: what needs
 * you and what runs are accent, done is green, failed is red. A project's monogram and colour are
 * in `identity.tsx`.
 */

import { Workflow } from "lucide-react";
import { isBusy, type Request, type Run, type Thread } from "@/lib/agents-api";

/** What an orchestrator has spun up: the agents it started (delegated threads) and how many still run. */
export function orchestration(thread: Thread, threads: Iterable<Thread>): { started: number; running: number } {
  let started = 0;
  let running = 0;
  for (const t of threads) {
    if (t.lineage?.parent !== thread.id || t.lineage.relation !== "subagent") continue;
    started++;
    if (isBusy(t.status)) running++;
  }
  return { started, running };
}

/** An orchestrator's mark (the palette's `Workflow` glyph), with the agents it started and how many still run. */
export function OrchestratorChip({ started, running, className = "" }: { started: number; running: number; className?: string }) {
  const label = `Orchestrator${started ? ` · ${started} agent${started === 1 ? "" : "s"}${running ? `, ${running} running` : ""}` : ""}`;
  return (
    <span aria-label={label} title={label} className={`inline-flex min-w-0 shrink-0 items-center gap-1 rounded-full border border-line-2 px-1.5 py-px text-[10.5px] leading-[16px] text-muted ${className}`}>
      <Workflow className={`size-3 shrink-0 ${running ? "text-accent" : ""}`} aria-hidden />
      <span aria-hidden>Orchestrator</span>
      {started > 0 && (
        <span aria-hidden className="font-mono tabular-nums text-subtle">
          {started}
          {running > 0 && <span className="text-accent"> · {running} running</span>}
        </span>
      )}
    </span>
  );
}

export type PillTone = "accent" | "ok" | "danger" | "muted";

export interface ThreadPill {
  label: string;
  tone: PillTone;
  /** Only running work moves. */
  pulse: boolean;
}

/** A run that has ended (it can't change any more). */
export function runEnded(run: Run | undefined): boolean {
  return !!run && ["completed", "interrupted", "failed", "cancelled", "rolled_back"].includes(run.status);
}

/**
 * The one status worth saying about a thread, T3's order: what waits on you first, then what's
 * working, then what failed, a plan ready to carry out, and a finished run you haven't looked at.
 * `null` when there's nothing to say (idle and seen). `planReady`: the newest run ended with a plan
 * in plan mode (`readyPlan`, known where the thread's nodes are loaded).
 */
export function threadPill(thread: Thread, pending: readonly Request[], lastRun: Run | undefined, planReady = false): ThreadPill | null {
  const waiting = pending.filter((r) => r.status === "pending" && r.thread === thread.id);
  const approval = waiting.find((r) => r.body.type === "approval");
  if (approval && approval.body.type === "approval" && approval.body.action === "plan") return { label: "Plan ready", tone: "accent", pulse: false };
  if (approval) return { label: "Pending approval", tone: "accent", pulse: false };
  if (waiting.length > 0) return { label: "Awaiting input", tone: "accent", pulse: false };
  if (thread.status === "running" || thread.status === "waiting") return { label: "Working", tone: "accent", pulse: true };
  if (thread.status === "starting") return { label: "Connecting", tone: "accent", pulse: true };
  if (thread.status === "error" || lastRun?.status === "failed") return { label: "Failed", tone: "danger", pulse: false };
  if (planReady) return { label: "Plan ready", tone: "accent", pulse: false };
  if (lastRun?.status === "completed" && unseen(thread, lastRun)) return { label: "Completed", tone: "ok", pulse: false };
  return null;
}

/** Finished since you last opened it. */
function unseen(thread: Thread, run: Run): boolean {
  const seen = thread.inbox?.visited_at ?? null;
  return seen === null || (run.completed_at ?? 0) > seen;
}

/**
 * Done and not looked at yet: idle, its last run completed since you last opened it (T3's
 * completed tone; the card's border is green until you open it, as T3's row pill is).
 */
export function threadDone(thread: Thread, lastRun: Run | undefined): boolean {
  return thread.status === "idle" && !thread.error && lastRun?.status === "completed" && thread.archived_at === null && unseen(thread, lastRun);
}

const toneText: Record<PillTone, string> = { accent: "text-accent", ok: "text-ok", danger: "text-danger", muted: "text-subtle" };
const toneDot: Record<PillTone, string> = { accent: "bg-accent", ok: "bg-ok", danger: "bg-danger", muted: "bg-subtle" };

/** T3's `ThreadStatusLabel`: a dot and the label, in the pill's tone. */
export function StatusPill({ pill, className = "" }: { pill: ThreadPill; className?: string }) {
  return (
    <span role="img" aria-label={pill.label} title={pill.label} className={`inline-flex shrink-0 items-center gap-1 text-[10.5px] font-medium ${toneText[pill.tone]} ${className}`}>
      <span aria-hidden className={`size-1.5 rounded-full ${toneDot[pill.tone]} ${pill.pulse ? "pulse-dot" : ""}`} />
      <span aria-hidden>{pill.label}</span>
    </span>
  );
}
