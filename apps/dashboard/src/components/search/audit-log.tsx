import { RefreshCw } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { searchApi, type SearchCluster } from "@/lib/search-api";
import { AUDIT_EVENTS, auditRow, eventLabel, type AuditEntry } from "@/lib/search-audit";
import { Segmented } from "../redis/shared";
import { btn, input, Panel } from "../ui";

const RANGES = [
  { label: "1h", value: 3_600_000 },
  { label: "24h", value: 86_400_000 },
  { label: "7d", value: 7 * 86_400_000 },
];

const PAGE = 100;

function when(ms: number) {
  return new Date(ms).toLocaleString("en-US", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false });
}

/**
 * The cluster's security audit log: failed logins, denied requests and security changes, newest
 * first, filtered by time and event. Reading it never wakes the cluster (entries are in object
 * storage), so it loads as the tab opens.
 */
export function AuditLog({ cluster }: { cluster: SearchCluster }) {
  const [range, setRange] = useState(RANGES[1].value);
  const [event, setEvent] = useState("");
  const [entries, setEntries] = useState<AuditEntry[] | null>(null);
  const [more, setMore] = useState(false);
  const [retention, setRetention] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const opensearch = cluster.flavor === "opensearch";

  const load = useCallback(
    async (before?: number) => {
      setLoading(true);
      try {
        const page = await searchApi.audit(cluster.id, { from: Date.now() - range, to: before, event: event || undefined, size: PAGE });
        setEntries((prev) => (before !== undefined && prev ? [...prev, ...page.entries] : page.entries));
        setMore(page.more);
        setRetention(page.retention_days);
        setError(null);
      } catch (e) {
        setError(e instanceof Error ? e.message : String(e));
      } finally {
        setLoading(false);
      }
    },
    [cluster.id, range, event],
  );

  useEffect(() => {
    load();
  }, [load]);

  const rows = entries?.map(auditRow) ?? null;
  const oldest = entries?.length ? entries[entries.length - 1].at_ms : undefined;
  return (
    <Panel
      title="Audit log"
      aside={
        <span className="flex flex-wrap items-center gap-2">
          <select value={event} onChange={(e) => setEvent(e.target.value)} className={`${input} py-1 text-[12px]`} aria-label="Event">
            <option value="">All events</option>
            {AUDIT_EVENTS[opensearch ? "opensearch" : "elasticsearch"].map((e) => (
              <option key={e} value={e}>
                {eventLabel(e)}
              </option>
            ))}
          </select>
          <Segmented value={range} options={RANGES} onChange={setRange} />
          <button onClick={() => load()} disabled={loading} className={btn} aria-label="Refresh">
            <RefreshCw className={`size-3.5 ${loading ? "animate-spin motion-reduce:animate-none" : ""}`} />
          </button>
        </span>
      }
    >
      {error && <p className="px-5 py-4 text-[13px] text-danger">{error}</p>}
      {rows === null && !error ? (
        <p className="px-5 py-6 text-[13px] text-subtle">
          <span className="shimmer">Reading the audit log…</span>
        </p>
      ) : rows && rows.length === 0 ? (
        <p className="px-5 py-6 text-[13px] text-muted">
          Nothing {event ? `of ${eventLabel(event)} ` : ""}in this period. Failed logins, denied requests and changes to users, roles and keys show up here.
        </p>
      ) : (
        rows && (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Time</th>
                  <th className="px-3 py-2 font-normal">Event</th>
                  <th className="px-3 py-2 font-normal">User</th>
                  <th className="px-3 py-2 font-normal">From</th>
                  <th className="px-3 py-2 font-normal">Action</th>
                  <th className="px-5 py-2 font-normal">On</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((r, i) => (
                  <tr key={`${r.at_ms}-${i}`} className="rise border-b border-line last:border-0">
                    <td className="whitespace-nowrap px-5 py-2.5 font-mono text-[12px] text-muted tabular-nums">{when(r.at_ms)}</td>
                    <td className="whitespace-nowrap px-3 py-2.5">
                      <span className="flex items-center gap-1.5 text-[12px]">
                        <span className={`inline-block size-2 rounded-full ${r.failure ? "bg-danger" : "bg-line-2"}`} />
                        {eventLabel(r.event)}
                      </span>
                    </td>
                    <td className="max-w-48 truncate px-3 py-2.5 font-mono text-[12px]">{r.user ?? <span className="text-subtle">—</span>}</td>
                    <td className="whitespace-nowrap px-3 py-2.5 font-mono text-[12px] text-muted">{r.address ?? <span className="text-subtle">—</span>}</td>
                    <td className="max-w-72 truncate px-3 py-2.5 font-mono text-[12px]" title={r.action ?? undefined}>
                      {r.action ?? <span className="text-subtle">—</span>}
                    </td>
                    <td className="max-w-64 truncate px-5 py-2.5 font-mono text-[12px] text-muted" title={r.target ?? undefined}>
                      {r.target ?? <span className="text-subtle">—</span>}
                    </td>
                  </tr>
                ))}
              </tbody>
            </table>
            {more && (
              <div className="border-t border-line px-5 py-3">
                <button onClick={() => load(oldest)} disabled={loading} className={btn}>
                  {loading ? <span className="shimmer">Loading…</span> : "Older entries"}
                </button>
              </div>
            )}
          </div>
        )
      )}
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        Kept {retention ?? 7} days. Never holds a password, key, token or request body. Granted requests are recorded only when the cluster asks:{" "}
        <span className="font-mono">{opensearch ? "_plugins/_security/api/audit" : "xpack.security.audit.logfile.events.include"}</span>.
      </p>
    </Panel>
  );
}
