/**
 * A thread's context window, read the way T3 Code's `ContextWindowMeter` reads it
 * (`apps/web/src/lib/contextWindow.ts`, `ContextWindowMeter.tsx`, MIT): a percentage only when
 * the window is known, tokens otherwise; "nearly full" past 90 %.
 */
import type { ContextUsage } from "./agents-api";

export interface ContextReading {
  used: number;
  max: number | null;
  /** 0–100, or `null` when the window is unknown. */
  percent: number | null;
  /** More than 90 % used (T3's `isOverloaded`). */
  full: boolean;
  /** "62%" / "4.5%", or `null` when the window is unknown. */
  percentLabel: string | null;
  /** "62% of 200k", or "8.5k tokens" when the window is unknown. */
  label: string;
  /** "124k/200k", or "8.5k". */
  tokens: string;
  /** "Context window 62% used" (T3's aria-label). */
  aria: string;
  processed: number | null;
}

export function readContext(c: ContextUsage): ContextReading {
  const used = Math.max(0, c.used_tokens);
  const max = c.max_tokens != null && c.max_tokens > 0 ? c.max_tokens : null;
  const percent = max === null ? null : Math.min(100, (used / max) * 100);
  const percentLabel = percent === null ? null : formatPercent(percent);
  return {
    used,
    max,
    percent,
    full: percent !== null && percent > 90,
    percentLabel,
    label: percentLabel === null || max === null ? `${formatContextTokens(used)} tokens` : `${percentLabel} of ${formatContextTokens(max)}`,
    tokens: max === null ? formatContextTokens(used) : `${formatContextTokens(used)}/${formatContextTokens(max)}`,
    aria: percentLabel === null ? `Context window ${formatContextTokens(used)} tokens used` : `Context window ${percentLabel} used`,
    processed: c.total_processed_tokens != null && c.total_processed_tokens > 0 ? c.total_processed_tokens : null,
  };
}

/** T3's `formatContextWindowTokens`: 999, 1.5k, 45k, 1.2M. */
export function formatContextTokens(n: number): string {
  if (!Number.isFinite(n)) return "0";
  if (n < 1_000) return `${Math.round(n)}`;
  if (n < 10_000) return `${(n / 1_000).toFixed(1).replace(/\.0$/, "")}k`;
  if (n < 999_500) return `${Math.round(n / 1_000)}k`;
  return `${(n / 1_000_000).toFixed(1).replace(/\.0$/, "")}M`;
}

/** T3's `formatPercentage`: one decimal under 10 %, whole numbers above. */
export function formatPercent(p: number): string {
  return p < 10 ? `${p.toFixed(1).replace(/\.0$/, "")}%` : `${Math.round(p)}%`;
}
