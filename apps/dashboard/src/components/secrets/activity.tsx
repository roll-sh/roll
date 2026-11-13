import { Eye, GitPullRequest, KeyRound, Pencil, RefreshCw, RotateCw, Search, Shield } from "lucide-react";
import { useEffect, useState } from "react";
import type { AuditEntry, Environment } from "@/lib/secrets-api";
import { useSecrets } from "./secrets-page";
import { ActorMark, ago, button, Chip, ErrorNote, errorText, sized } from "./ui";

type Kind = "all" | "changes" | "reads" | "requests" | "syncs";

const KIND_OF = (action: string): Exclude<Kind, "all"> | "other" =>
  action.startsWith("secrets.reveal") || action === "secrets.export" || action === "secrets.resolve" || action === "request.reveal" || action.startsWith("share.")
    ? "reads"
    : action.startsWith("request.")
      ? "requests"
      : action.startsWith("sync.")
        ? "syncs"
        : action.startsWith("secrets.") || action.startsWith("environment.") || action.startsWith("imports.") || action.startsWith("exports.") || action.startsWith("folder.")
          ? "changes"
          : "other";

const ICON: Record<ReturnType<typeof KIND_OF>, typeof Pencil> = { changes: Pencil, reads: Eye, requests: GitPullRequest, syncs: RefreshCw, other: Shield };

const day = (ms: number) => new Date(ms).toLocaleDateString(undefined, { weekday: "short", month: "short", day: "numeric" });

/**
 * An environment's activity (docs/secrets.md, "Audit log"): its changes, reads, change requests and
 * syncs' pushes as one timeline, newest first, filterable by key: who changed what, who read it, and
 * where it went. Names and summaries only; the audit log never holds a value.
 */
export function Activity({ env }: { env: Environment }) {
  const { api, go } = useSecrets();
  const [key, setKey] = useState("");
  const [kind, setKind] = useState<Kind>("all");
  const [rows, setRows] = useState<AuditEntry[] | null>(null);
  const [more, setMore] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const filter = { env: env.slug, key: key.trim() || undefined, limit: 100 };
  const load = (before?: number) =>
    api.audit({ ...filter, before }).then(
      (r) => {
        setRows((old) => (before ? [...(old ?? []), ...r] : r));
        setMore(r.length === 100);
      },
      (e) => setError(errorText(e)),
    );
  useEffect(() => {
    void load();
    // A new filter starts over.
  }, [api, env.slug, key]);

  const shown = (rows ?? []).filter((r) => kind === "all" || KIND_OF(r.action) === kind);
  let lastDay = "";
  return (
    <div className="px-3 pt-3 pb-8">
      <div className="mb-3 flex flex-wrap items-center gap-2">
        <label className="flex h-7 w-56 items-center gap-1.5 rounded-md border border-line-2 bg-panel-2 px-2 text-subtle focus-within:border-accent/60">
          <Search className="size-3.5" />
          <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="Key" aria-label="Activity of a key" spellCheck={false} className={`${sized} h-auto min-w-0 flex-1 border-0 bg-transparent p-0 font-mono text-[12px]`} />
        </label>
        {(["all", "changes", "reads", "requests", "syncs"] as const).map((k) => (
          <Chip key={k} on={kind === k} onClick={() => setKind(k)}>
            {k === "all" ? "Everything" : k[0].toUpperCase() + k.slice(1)}
          </Chip>
        ))}
      </div>
      <ErrorNote error={error} onDismiss={() => setError(null)} className="mb-2" />
      {rows && shown.length === 0 && <p className="rounded-xl border border-dashed border-line-2 px-3 py-4 text-[12px] text-muted">Nothing yet{key ? ` for ${key}` : ""}. Every change, read, request and sync push in {env.name} shows here.</p>}
      <ol className="space-y-0.5">
        {shown.map((r) => {
          const d = day(r.at);
          const head = d !== lastDay;
          lastDay = d;
          const Icon = ICON[KIND_OF(r.action)];
          return (
            <li key={r.id}>
              {head && <p className="mt-3 mb-1 text-[11px] font-medium text-subtle first:mt-0">{d}</p>}
              <div className="flex flex-wrap items-baseline gap-x-2 rounded-md px-1.5 py-1 text-[12.5px] hover:bg-fg/[0.03]">
                <Icon className="size-3.5 shrink-0 translate-y-0.5 text-muted" />
                <span className="text-soft">{r.summary}</span>
                {r.keys.slice(0, 6).map((k) => (
                  <button key={k} className="font-mono text-[11px] text-muted transition-colors hover:text-accent hover:underline" onClick={() => go({ env: env.slug, path: r.path && r.path !== "/" ? r.path : undefined, key: k })}>
                    {k}
                  </button>
                ))}
                {r.keys.length > 6 && <span className="text-[11px] text-subtle">+{r.keys.length - 6}</span>}
                <span className="ml-auto flex items-center gap-1.5 text-[11px] whitespace-nowrap text-subtle">
                  <ActorMark who={r.actor} />
                  <span title={new Date(r.at).toLocaleString()}>{ago(r.at)}</span>
                  {r.action.startsWith("secrets.rotate") || r.action.endsWith(".rotate") ? <RotateCw className="size-3" /> : r.action.startsWith("lease.") ? <KeyRound className="size-3" /> : null}
                </span>
              </div>
            </li>
          );
        })}
      </ol>
      {more && rows && rows.length > 0 && (
        <button className={`${button} mt-3`} onClick={() => load(rows[rows.length - 1].id)}>
          Older
        </button>
      )}
    </div>
  );
}
