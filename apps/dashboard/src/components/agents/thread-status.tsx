"use client";

/**
 * A thread's status as T3 Code shows it on its rows (T3's `Sidebar.logic.ts`
 * `resolveThreadStatusPill` and `ThreadStatusIndicators.tsx`, MIT), and the project's monogram
 * (T3's `ProjectMonogram.tsx` / `projectIdentity.ts`). In roll's colours: what needs you and what
 * runs are accent, done is green, failed is red; the monogram is monochrome (T3 tints it from a
 * palette with orange in it).
 */

import type { Request, Run, Thread } from "@/lib/agents-api";

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
 * `null` when there's nothing to say (idle and seen).
 */
export function threadPill(thread: Thread, pending: readonly Request[], lastRun: Run | undefined): ThreadPill | null {
  const waiting = pending.filter((r) => r.status === "pending" && r.thread === thread.id);
  const approval = waiting.find((r) => r.body.type === "approval");
  if (approval && approval.body.type === "approval" && approval.body.action === "plan") return { label: "Plan ready", tone: "accent", pulse: false };
  if (approval) return { label: "Pending approval", tone: "accent", pulse: false };
  if (waiting.length > 0) return { label: "Awaiting input", tone: "accent", pulse: false };
  if (thread.status === "running" || thread.status === "waiting") return { label: "Working", tone: "accent", pulse: true };
  if (thread.status === "starting") return { label: "Connecting", tone: "accent", pulse: true };
  if (thread.status === "error" || lastRun?.status === "failed") return { label: "Failed", tone: "danger", pulse: false };
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

/** T3's monogram: the first letter and the first digit (or the last word's first letter), two at most. */
export function monogram(name: string): string {
  const words = name.normalize("NFKC").trim().match(/[\p{L}\p{N}]+/gu) ?? [];
  const first = words[0];
  if (!first) return "PR";
  const glyphs = Array.from(first);
  const a = glyphs[0] ?? "P";
  const b = glyphs.slice(1).find((g) => /\p{N}/u.test(g)) ?? (words.length > 1 ? Array.from(words.at(-1) ?? "")[0] : glyphs.at(-1)) ?? a;
  return Array.from(`${a}${b}`.toUpperCase()).slice(0, 2).join("");
}

/** A project's tile: its monogram in a rounded square (T3's `ProjectMonogram`), monochrome. */
export function ProjectMonogram({ name, className = "size-4" }: { name: string; className?: string }) {
  const text = monogram(name);
  return (
    <span aria-hidden className={`inline-flex shrink-0 items-center justify-center ${className}`}>
      <svg viewBox="0 0 16 16" className="size-full overflow-hidden rounded-[25%] text-muted select-none" style={{ backgroundColor: "color-mix(in srgb, currentColor 16%, transparent)" }}>
        <text
          x="8"
          y="10.8"
          textAnchor="middle"
          fill="currentColor"
          className="font-mono"
          fontSize="8.25"
          fontWeight="700"
          textLength={Array.from(text).length === 1 ? 6 : 12}
          lengthAdjust="spacingAndGlyphs"
          textRendering="geometricPrecision"
        >
          {text}
        </text>
      </svg>
    </span>
  );
}
