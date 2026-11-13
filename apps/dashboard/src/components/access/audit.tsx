import { ChevronRight } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { accessApi, ACTIONS, productName, type AuditEvent, type AuditFilter } from "@/lib/access-api";
import { btn, Connecting, EmptyPanel, ErrorNote, input } from "../ui";
import { AccessShell, relative } from "./shell";
import { Sinks } from "./sinks";

const PAGE = 100;

/**
 * `/dashboard/access/audit`: every request a token made and every MCP tool call, newest first,
 * with the decision, the resource, and the arguments as stored (secrets redacted).
 */
export function Audit({ grant }: { grant?: string }) {
  const [filter, setFilter] = useState<AuditFilter>({ grant });
  const [events, setEvents] = useState<AuditEvent[] | null>(null);
  const [more, setMore] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(
    async (before?: number) => {
      try {
        const page = await accessApi.audit({ ...filter, before, limit: PAGE });
        setEvents((old) => (before && old ? [...old, ...page] : page));
        setMore(page.length === PAGE);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      }
    },
    [filter],
  );
  useEffect(() => {
    void load();
    const t = setInterval(() => document.visibilityState === "visible" && void load(), 15_000);
    return () => clearInterval(t);
  }, [load]);

  const download = async () => {
    try {
      const blob = await accessApi.auditExport({ ...filter, limit: 1000 });
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = "roll-audit.jsonl";
      a.click();
      URL.revokeObjectURL(url);
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const set = (k: keyof AuditFilter) => (e: { target: { value: string } }) => setFilter((f) => ({ ...f, [k]: e.target.value || undefined }));
  return (
    <AccessShell tab="audit">
      <div className="space-y-4">
        <Sinks />
        <div className="flex flex-wrap gap-2">
          <input value={filter.grant ?? ""} onChange={set("grant")} placeholder="Token (tok_…)" className={`${input} w-44 font-mono text-[12px]`} />
          <input value={filter.product ?? ""} onChange={set("product")} placeholder="Product" className={`${input} w-32`} />
          <input value={filter.resource ?? ""} onChange={set("resource")} placeholder="Resource (who touched it)" className={`${input} w-52 font-mono text-[12px]`} />
          <select value={filter.action ?? ""} onChange={set("action")} className={input} aria-label="Action">
            <option value="">Any action</option>
            {ACTIONS.map((a) => (
              <option key={a} value={a}>
                {a}
              </option>
            ))}
          </select>
          <select value={filter.decision ?? ""} onChange={set("decision")} className={input}>
            <option value="">Any decision</option>
            <option value="allow">Allowed</option>
            <option value="ask">Asked</option>
            <option value="deny">Denied</option>
          </select>
          <select value={filter.source ?? ""} onChange={set("source")} className={input}>
            <option value="">API and MCP</option>
            <option value="mcp">MCP tool calls</option>
            <option value="api">API requests</option>
          </select>
          <button onClick={() => void download()} className={`${btn} ml-auto`} title="The newest 1,000 matching events, one JSON object per line">
            Export
          </button>
        </div>
        <ErrorNote error={error} />
        {events === null ? (
          !error && <Connecting />
        ) : events.length === 0 ? (
          <EmptyPanel>
            <h2 className="text-[15px] font-medium">Nothing yet</h2>
            <p className="max-w-md text-[13px] text-muted">Requests made with tokens and every MCP tool call appear here, kept for 90 days.</p>
          </EmptyPanel>
        ) : (
          <div className="overflow-hidden rounded-xl border border-line bg-panel">
            <ul className="divide-y divide-line">
              {events.map((e) => (
                <Row key={e.id} e={e} onResource={(resource) => setFilter((f) => ({ ...f, resource, grant: undefined }))} />
              ))}
            </ul>
            {more && (
              <div className="border-t border-line p-3">
                <button onClick={() => load(events[events.length - 1]?.at)} className={btn}>
                  Older
                </button>
              </div>
            )}
          </div>
        )}
      </div>
    </AccessShell>
  );
}

const DECISION: Record<AuditEvent["decision"], string> = { allow: "text-muted", ask: "text-accent", deny: "text-danger" };

function Row({ e, onResource }: { e: AuditEvent; onResource: (resource: string) => void }) {
  const [open, setOpen] = useState(false);
  const what = e.tool ?? `${e.method} ${e.route}`;
  return (
    <li>
      <button onClick={() => setOpen((o) => !o)} aria-expanded={open} className="flex w-full items-center gap-3 px-4 py-2 text-left text-[12px] hover:bg-white/[0.02]">
        <ChevronRight className={`size-3 shrink-0 text-subtle transition-transform duration-150 ${open ? "rotate-90" : ""}`} />
        <span className="w-16 shrink-0 text-subtle">{relative(e.at)}</span>
        <span className={`w-12 shrink-0 ${DECISION[e.decision]}`}>{e.decision}</span>
        <span className="w-28 shrink-0 truncate">{e.grant_name ?? "operator"}</span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px]">{what}</span>
        <span className="hidden w-40 shrink-0 truncate text-muted sm:block">
          {e.product ? productName(e.product) : ""} {e.resource ?? ""}
        </span>
        <span className={`w-10 shrink-0 text-right font-mono ${e.status >= 400 ? "text-danger" : "text-subtle"}`}>{e.status}</span>
      </button>
      {open && (
        <div className="rise space-y-2 px-11 pb-3 text-[12px] text-muted">
          <p className="font-mono text-[11px]">
            {e.method} {e.route} · {e.action ?? "—"} · {e.source} · {e.duration_ms} ms{e.ip ? ` · ${e.ip}` : ""}
            {e.client && (
              <>
                {" · says it's "}
                <bdi>{e.client}</bdi>
              </>
            )}
            {e.approval ? ` · approval ${e.approval}` : ""} · {new Date(e.at).toISOString()}
            {e.resource && (
              <>
                {" · "}
                <button onClick={() => onResource(e.resource!)} className="text-accent hover:underline">
                  who else touched {e.resource}
                </button>
              </>
            )}
          </p>
          {e.args !== null && e.args !== undefined && (
            <pre className="max-h-48 overflow-auto rounded-lg border border-line bg-bg p-2.5 font-mono text-[11px]">{JSON.stringify(e.args, null, 2)}</pre>
          )}
        </div>
      )}
    </li>
  );
}
