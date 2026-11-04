"use client";

import type { ContextUsage } from "@/lib/agents-api";
import { formatContextTokens, readContext } from "@/lib/agents-context";

const R = 9.75;
const CIRCUMFERENCE = 2 * Math.PI * R;

/**
 * How full a conversation's context window is: a small ring that fills as context is used, with
 * the details on hover or focus (T3 Code's `ContextWindowMeter`). Monochrome until more than
 * 90 % is used, then accent. A window the provider didn't name shows tokens, never a percentage.
 */
export function ContextMeter({
  context,
  size = 14,
  label = false,
  align = "end",
}: {
  context: ContextUsage;
  /** The ring's size in px. */
  size?: number;
  /** Show the percentage beside the ring (thread cards). */
  label?: boolean;
  align?: "start" | "end";
}) {
  const c = readContext(context);
  const fill = c.percent ?? 0;
  return (
    <span className="group/ctx relative inline-flex shrink-0 items-center gap-1 outline-none" tabIndex={0} role="img" aria-label={c.aria}>
      <svg viewBox="0 0 24 24" width={size} height={size} className="-rotate-90" aria-hidden="true">
        <circle cx="12" cy="12" r={R} fill="none" strokeWidth="3" className="stroke-line-2" />
        <circle
          cx="12"
          cy="12"
          r={R}
          fill="none"
          strokeWidth="3"
          strokeLinecap="round"
          strokeDasharray={CIRCUMFERENCE}
          strokeDashoffset={CIRCUMFERENCE * (1 - fill / 100)}
          className={`transition-[stroke-dashoffset,stroke] duration-300 ease-out motion-reduce:transition-none ${c.full ? "stroke-accent" : "stroke-muted"} ${fill === 0 ? "opacity-0" : ""}`}
        />
      </svg>
      {label && c.percentLabel && <span className={`font-mono tabular-nums ${c.full ? "text-accent" : ""}`}>{c.percentLabel}</span>}
      <span
        role="tooltip"
        className={`pointer-events-none absolute bottom-full z-20 mb-1.5 w-max min-w-52 max-w-72 rounded-md border border-line-2 bg-panel px-2.5 py-2 text-left font-sans text-[11px] text-muted opacity-0 transition-opacity duration-150 group-hover/ctx:opacity-100 group-focus-visible/ctx:opacity-100 motion-reduce:transition-none ${
          align === "end" ? "right-0" : "left-0"
        }`}
      >
        <span className="flex items-center justify-between gap-4 whitespace-nowrap">
          <span className="font-medium text-fg">Context window</span>
          <span className="font-mono tabular-nums">{c.percentLabel ? `${c.percentLabel} · ${c.tokens}` : c.tokens}</span>
        </span>
        {c.percent !== null ? (
          <span className="mt-1.5 block h-1 overflow-hidden rounded-full bg-line" aria-hidden="true">
            <span className={`block h-full rounded-full ${c.full ? "bg-accent" : "bg-muted"}`} style={{ width: `${c.percent}%` }} />
          </span>
        ) : (
          <span className="mt-1 block text-subtle">The model&apos;s window isn&apos;t known.</span>
        )}
        {c.processed !== null && (
          <span className="mt-1.5 flex justify-between gap-3">
            <span>Total processed</span>
            <span className="font-mono tabular-nums">{formatContextTokens(c.processed)}</span>
          </span>
        )}
      </span>
    </span>
  );
}
