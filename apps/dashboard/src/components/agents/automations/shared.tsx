"use client";

/**
 * What the Automations view's parts share: the store hook, trigger and step icons, the run status
 * dot and pill, the seven-day strip, and small buttons. docs/code/automations-ui.md is the design.
 */

import {
  Bell,
  Bot,
  CalendarClock,
  Check,
  CircleDashed,
  Clock,
  GitBranch,
  GitPullRequestArrow,
  Hand,
  Hourglass,
  MessageSquareReply,
  MessagesSquare,
  Play,
  Plug,
  Repeat,
  ShieldCheck,
  SquareTerminal,
  Webhook,
  X,
  Archive,
  Zap,
  type LucideIcon,
} from "lucide-react";
import { useMemo, type ComponentProps, type ReactNode } from "react";
import type { RunState, StepState } from "@/lib/automations-api";
import { automationsFor, useAutomationsState, type AutomationsState, type AutomationsStore } from "@/lib/automations-store";
import { runLabel, runTone, withBuiltIns, type Tone } from "@/lib/automations-ui";
import { cn, Select } from "../ui";
import { useAgents } from "../use-agents";

/** The owner's automations store, or `null` where there are none (a share's replay, old tests). */
export function useAutomationsStore(): AutomationsStore | null {
  const { client, services } = useAgents();
  const api = services?.automations;
  return useMemo(() => (api ? automationsFor(client, api) : null), [client, api]);
}

/** The store's state and the catalog with the built-ins. */
export function useAutomations(): AutomationsState & { store: AutomationsStore | null; catalog: NonNullable<AutomationsState["catalog"]> } {
  const store = useAutomationsStore();
  const state = useAutomationsState(store);
  const catalog = useMemo(() => withBuiltIns(state.catalog), [state.catalog]);
  return { ...state, store, catalog };
}

const triggerIcons: Record<string, LucideIcon> = {
  manual: Hand,
  schedule: CalendarClock,
  interval: Repeat,
  webhook: Webhook,
  code: GitBranch,
  agent: Bot,
};

export const triggerIcon = (source: string): LucideIcon => triggerIcons[source] ?? Plug;

const stepIcons: Record<string, LucideIcon> = {
  "agent.start": Play,
  "agent.wait": Hourglass,
  "agent.message": MessagesSquare,
  "agent.respond": ShieldCheck,
  "agent.publish": GitPullRequestArrow,
  "agent.script": SquareTerminal,
  "agent.settle": Archive,
  webhook: Webhook,
  notify: Bell,
  delay: Clock,
};

export const stepIcon = (kind: string): LucideIcon => stepIcons[kind] ?? (kind.endsWith(".comment") || kind.endsWith(".post") ? MessageSquareReply : Plug);

export { Zap as AutomationIcon };

/** An icon on a raised square, as the canvas cards lead with (design spec: IconTile). */
export function Tile({ icon: Icon, className, tone }: { icon: LucideIcon; className?: string; tone?: Tone }) {
  return (
    <span
      className={cn(
        "grid size-7 shrink-0 place-items-center rounded-lg border border-line bg-panel-2 inset-shadow-highlight",
        tone === "accent" ? "text-accent" : tone === "danger" ? "text-danger" : tone === "ok" ? "text-ok" : "text-muted",
        className,
      )}
      aria-hidden
    >
      <Icon className="size-3.5" strokeWidth={1.75} />
    </span>
  );
}

const dotTone: Record<Tone, string> = { accent: "bg-accent", ok: "bg-ok", danger: "bg-danger", muted: "bg-subtle" };
const textTone: Record<Tone, string> = { accent: "text-accent", ok: "text-ok", danger: "text-danger", muted: "text-subtle" };

/** A run's state as a 6px dot (busy ones breathe). */
export function RunDot({ status, className }: { status: RunState; className?: string }) {
  const tone = runTone[status];
  return <span className={cn("inline-block size-1.5 shrink-0 rounded-full", dotTone[tone], tone === "accent" && "pulse-dot", className)} aria-hidden />;
}

/** A run's state in words with its dot. */
export function RunStatus({ status, className }: { status: RunState; className?: string }) {
  return (
    <span className={cn("inline-flex items-center gap-1.5 text-[12px]", textTone[runTone[status]], status === "running" && "shimmer", className)}>
      <RunDot status={status} />
      {runLabel[status]}
    </span>
  );
}

/** A step's state as the timeline's 16px glyph. */
export function StepGlyph({ status }: { status: StepState }) {
  const base = "grid size-[18px] shrink-0 place-items-center rounded-full border";
  switch (status) {
    case "succeeded":
      return (
        <span className={cn(base, "border-ok/40 bg-ok/12 text-ok")} aria-label="Done">
          <Check className="size-2.5" strokeWidth={2.5} />
        </span>
      );
    case "failed":
      return (
        <span className={cn(base, "border-danger/45 bg-danger/12 text-danger")} aria-label="Failed">
          <X className="size-2.5" strokeWidth={2.5} />
        </span>
      );
    case "running":
    case "waiting":
    case "retrying":
      return (
        <span className={cn(base, "border-accent/50 bg-accent/10")} aria-label={status === "waiting" ? "Waiting" : status === "retrying" ? "Retrying" : "Running"}>
          <span className="pulse-dot size-1.5 rounded-full bg-accent" />
        </span>
      );
    case "cancelled":
    case "skipped":
      return (
        <span className={cn(base, "border-line-2 text-subtle")} aria-label={status === "skipped" ? "Skipped" : "Cancelled"}>
          <CircleDashed className="size-2.5" />
        </span>
      );
    default:
      return <span className={cn(base, "border-line-2 border-dashed")} aria-label="Pending" />;
  }
}

/** Seven days of runs as small bars (failed in red), with the count after. */
export function WeekStrip({ days, total, failed }: { days: { ok: number; failed: number }[]; total: number; failed: number }) {
  const max = Math.max(1, ...days.map((d) => d.ok + d.failed));
  return (
    <span className="inline-flex items-center gap-2" title={`${total} run${total === 1 ? "" : "s"} in the last 7 days${failed ? `, ${failed} failed` : ""}`} aria-label={`${total} runs in 7 days${failed ? `, ${failed} failed` : ""}`}>
      <span className="flex h-4 items-end gap-[2px]" aria-hidden>
        {days.map((d, i) => {
          const n = d.ok + d.failed;
          const h = n ? Math.max(4, Math.round((n / max) * 16)) : 0;
          // Each day a faint 16px track, filled from the bottom (failed days in red).
          return (
            <span key={i} className="relative h-4 w-1 overflow-hidden rounded-[1.5px] bg-fg/[0.06]">
              {n > 0 && <span className={cn("absolute inset-x-0 bottom-0 rounded-[1.5px]", d.failed ? "bg-danger/85" : "bg-fg/50")} style={{ height: h }} />}
            </span>
          );
        })}
      </span>
      <span className="w-6 text-right font-mono text-[11px] text-subtle tabular-nums">{total}</span>
    </span>
  );
}

/** A quiet text button (Linear's secondary): used in bars and cards. */
export function QuietButton({ children, onClick, disabled, title, className, type = "button", ariaLabel }: { children: ReactNode; onClick?: () => void; disabled?: boolean; title?: string; className?: string; type?: "button" | "submit"; ariaLabel?: string }) {
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      title={title}
      aria-label={ariaLabel}
      className={cn(
        "inline-flex h-7 items-center gap-1.5 rounded-md border border-line-2 bg-fg/[0.03] px-2.5 text-[12.5px] font-[510] text-soft inset-shadow-highlight transition-[background-color,border-color,color,scale] duration-150 hover:border-line-3 hover:bg-fg/[0.06] hover:text-fg active:scale-[0.97] disabled:pointer-events-none disabled:opacity-50 [&_svg]:size-3.5",
        className,
      )}
    >
      {children}
    </button>
  );
}

/** The primary button of the agents surface (white on dark, a highlight and a shadow). */
export function PrimaryButton({ children, onClick, disabled, title, className, type = "button" }: { children: ReactNode; onClick?: () => void; disabled?: boolean; title?: string; className?: string; type?: "button" | "submit" }) {
  return (
    <button
      type={type}
      onClick={onClick}
      disabled={disabled}
      title={title}
      className={cn(
        "inline-flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 text-[12.5px] font-medium tracking-[-0.01em] text-bg shadow-card inset-shadow-[0_1px_0_rgb(255_255_255/0.25)] transition-[opacity,scale] duration-150 hover:opacity-90 active:scale-[0.97] disabled:pointer-events-none disabled:opacity-40 [&_svg]:size-3.5",
        className,
      )}
    >
      {children}
    </button>
  );
}

/** "in 3 h", "in 12 min": when something next happens. */
export function inTime(at: number, now = Date.now()): string {
  const s = Math.round((at - now) / 1000);
  if (s <= 30) return "now";
  const m = Math.round(s / 60);
  if (m < 60) return `in ${m} min`;
  const h = Math.round(m / 60);
  if (h < 24) return `in ${h} h`;
  const d = Math.round(h / 24);
  return `in ${d} day${d === 1 ? "" : "s"}`;
}

/** An `ago` short enough for a list column: "now", "4m", "3h", "2d". */
export function shortAgo(at: number, now = Date.now()): string {
  const s = Math.max(0, Math.round((now - at) / 1000));
  if (s < 45) return "now";
  const m = Math.round(s / 60);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

export const ICON_STROKE = 1.75;

/**
 * T3's `Select` at a width: its trigger is `w-full`, and `cn` doesn't merge classes, so a width
 * goes on a box around it instead.
 */
export function SizedSelect<V extends string>({ width, ...props }: ComponentProps<typeof Select<V>> & { width: string }) {
  return (
    <span className={cn("inline-block min-w-0 [&>button]:min-w-0", width)}>
      <Select<V> {...props} />
    </span>
  );
}
