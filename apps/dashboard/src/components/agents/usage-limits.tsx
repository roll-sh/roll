/**
 * A provider account's usage limits: one row per window with what's left, a hairline bar with the
 * pace marker and when it resets (T3's `apps/web/src/components/usage/UsageLimits.tsx`,
 * `LimitWindows`). Shown on the provider's card in machine settings and in the model picker.
 */
import type { ProviderInfo } from "@/lib/agents-api";
import { aheadOfPace, pace, percentLeft, planLabel, resetsIn } from "@/lib/agents-usage";
import { cn } from "./ui";
import { useEffect, useState } from "react";

/** Now, ticking each minute so "resets in" stays true while the panel is open. */
function useNow(): number {
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 60_000);
    return () => clearInterval(t);
  }, []);
  return now;
}

export function UsageLimitBars({ info, className }: { info: Pick<ProviderInfo, "usage_limits" | "name">; className?: string }) {
  const now = useNow();
  const limits = info.usage_limits;
  if (!limits || limits.windows.length === 0) return null;
  const plan = planLabel(limits.plan);
  return (
    <div className={cn("space-y-2", className)} aria-label={`${info.name} usage limits`}>
      {limits.windows.map((w) => {
        const left = percentLeft(w);
        const out = w.used_percent >= 100;
        const fast = !out && aheadOfPace(w, now);
        const marker = pace(w, now);
        const reset = resetsIn(w, now);
        return (
          <div key={w.id} className="text-[12px]">
            <div className="flex items-baseline gap-2">
              <span className="text-muted">{w.label}</span>
              <span className={cn("font-mono tabular-nums", out ? "text-danger" : fast ? "text-accent" : "text-fg")}>{out ? "Limit reached" : `${left}% left`}</span>
              {reset && <span className="ml-auto text-subtle">{reset}</span>}
            </div>
            <div
              role="meter"
              aria-label={`${w.label} used`}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.round(w.used_percent)}
              className="relative mt-1 h-1 overflow-hidden rounded-full bg-fg/[0.08]"
            >
              <div
                className={cn("h-full rounded-full transition-[width] duration-300 ease-out motion-reduce:transition-none", out ? "bg-danger" : fast ? "bg-accent" : "bg-fg/50")}
                style={{ width: `${Math.min(100, Math.max(0, w.used_percent))}%` }}
              />
              {marker !== null && <div className="absolute inset-y-0 w-px bg-fg/40" style={{ left: `${marker}%` }} title="Even pace" />}
            </div>
          </div>
        );
      })}
      {plan && <p className="text-[11.5px] text-subtle">{plan} plan</p>}
    </div>
  );
}
