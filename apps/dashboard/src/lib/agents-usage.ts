/**
 * Usage limits as a person reads them (T3's `UsageLimits.tsx` `LimitWindows` and
 * `packages/shared/src/usageLimits.ts`): how much is left, when it resets, whether the pace runs
 * ahead of the window, and the window that's run out.
 */
import type { Millis, ProviderInfo, UsageLimits, UsageWindow } from "./agents-api";

/** Percent left, whole (T3 shows "N% left"). */
export function percentLeft(w: UsageWindow): number {
  return Math.max(0, Math.min(100, Math.round(100 - w.used_percent)));
}

/** "resets in 2h 10m", "resets in 3d", "resets soon"; nothing when it doesn't say. */
export function resetsIn(w: UsageWindow, now: Millis): string | null {
  if (w.resets_at === undefined) return null;
  const mins = Math.round((w.resets_at - now) / 60_000);
  if (mins <= 0) return "resets soon";
  if (mins < 60) return `resets in ${mins}m`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `resets in ${hours}h${mins % 60 ? ` ${mins % 60}m` : ""}`;
  const days = Math.floor(hours / 24);
  return `resets in ${days}d${hours % 24 ? ` ${hours % 24}h` : ""}`;
}

/**
 * Where the use "should" be by now if it were spread evenly over the window, 0 to 100 (T3's pace
 * marker); nothing without a reset time and a length.
 */
export function pace(w: UsageWindow, now: Millis): number | null {
  if (w.resets_at === undefined || !w.window_minutes) return null;
  const length = w.window_minutes * 60_000;
  const elapsed = length - (w.resets_at - now);
  if (elapsed < 0 || elapsed > length) return null;
  return Math.round((elapsed / length) * 100);
}

/** Ahead of pace by more than T3's 5 points: it will run out before it resets. */
export function aheadOfPace(w: UsageWindow, now: Millis): boolean {
  const p = pace(w, now);
  return p !== null && w.used_percent > p + 5;
}

/** The window that has run out (T3's `exhaustedUsageWindow`), the one resetting last. */
export function exhausted(limits: UsageLimits | undefined): UsageWindow | null {
  const out = (limits?.windows ?? []).filter((w) => w.used_percent >= 100);
  if (out.length === 0) return null;
  return out.reduce((a, b) => ((b.resets_at ?? Infinity) > (a.resets_at ?? Infinity) ? b : a));
}

/** The tightest window (least left), for a one-word summary in a narrow place. */
export function tightest(limits: UsageLimits | undefined): UsageWindow | null {
  const ws = limits?.windows ?? [];
  if (ws.length === 0) return null;
  return ws.reduce((a, b) => (b.used_percent > a.used_percent ? b : a));
}

/** "42% left" / "Out until 3d" for a provider, or nothing when it reports no limits. */
export function usageSummary(info: Pick<ProviderInfo, "usage_limits"> | undefined, now: Millis): string | null {
  const limits = info?.usage_limits;
  const out = exhausted(limits);
  if (out) {
    const when = resetsIn(out, now);
    return `${out.label} limit reached${when ? ` · ${when}` : ""}`;
  }
  const w = tightest(limits);
  return w ? `${percentLeft(w)}% left · ${w.label.toLowerCase()}` : null;
}

/** The plan's name as a person writes it ("plus" → "Plus"). */
export function planLabel(plan: string | undefined): string | null {
  if (!plan) return null;
  return plan.charAt(0).toUpperCase() + plan.slice(1);
}
