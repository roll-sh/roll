import { ArrowUpRight, ExternalLink } from "lucide-react";
import type { PreviewEnvironment } from "@/lib/code-api";
import type { PreviewKind } from "@/lib/previews-api";
import { PREVIEW_KIND_ICON } from "../workspace/preview-sheet";
import { host, money, STATE_LABEL, StateDot } from "../workspace/previews-common";

/**
 * A change's preview environment (roll previews): every service's URL, the data branches, what
 * it costs, and the way to its project's Previews tab. The change page's panel and its aside.
 */
export function PreviewEnvironmentView({ env, compact = false }: { env: PreviewEnvironment; compact?: boolean }) {
  return (
    <div className={compact ? "space-y-2" : "space-y-3"}>
      <p className="flex items-center gap-2 text-[12px] text-muted">
        <StateDot state={env.state} />
        {STATE_LABEL[env.state] ?? env.state}
        <span className="font-mono text-[11px] text-subtle">{env.slug}</span>
      </p>
      {env.services.length > 0 && (
        <ul className="space-y-1">
          {env.services.map((s) => (
            <li key={s.name} className="flex min-w-0 items-center gap-2 text-[12px]">
              <StateDot state={s.state} />
              <span className="shrink-0 text-muted">{s.name}</span>
              <a href={s.url} target="_blank" rel="noreferrer" className="flex min-w-0 items-center gap-1 font-mono text-[12px] text-accent hover:underline">
                <span className="truncate">{host(s.url)}</span>
                <ExternalLink className="size-3 shrink-0" />
              </a>
            </li>
          ))}
        </ul>
      )}
      {env.resources.length > 0 && (
        <ul className="space-y-1 text-[12px] text-subtle" aria-label="Data branches">
          {env.resources.map((r) => {
            const Icon = PREVIEW_KIND_ICON[r.kind as PreviewKind];
            return (
              <li key={`${r.kind}:${r.parent}`} className="flex min-w-0 items-center gap-2">
                {Icon && <Icon className="size-3.5 shrink-0" strokeWidth={1.5} />}
                <span className="truncate font-mono text-[11.5px]">{r.parent}</span>
                <span>→</span>
                <span className="truncate font-mono text-[11.5px] text-muted">{r.ref ?? "branching…"}</span>
              </li>
            );
          })}
        </ul>
      )}
      <p className="font-mono text-[11px] text-subtle tabular-nums">
        {money(env.sleeping_usd_month)}/mo asleep · {money(env.active_usd_hour)}/h active
      </p>
      {env.expires_at != null && (
        <p className="text-[12px] text-muted">
          Unused, it goes {new Date(env.expires_at).toLocaleString(undefined, { weekday: "short", hour: "numeric", minute: "2-digit" })} unless it's opened or kept.
        </p>
      )}
      {env.dashboard_url && (
        <a href={env.dashboard_url} className="inline-flex items-center gap-1 text-[12px] text-muted hover:text-fg">
          Open in Previews <ArrowUpRight className="size-3.5" strokeWidth={1.5} />
        </a>
      )}
    </div>
  );
}
