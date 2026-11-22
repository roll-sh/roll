import { Play, Plus, Square } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import { esJson, formatCount, type SearchCluster } from "@/lib/search-api";
import { POLL_MS } from "../redis/shared";
import { ago, btn, ErrorNote, Panel } from "../ui";
import { Empty, errText, LoadError, useLoad } from "./relevance-shared";

/** The connectors roll runs syncs for (Elastic-managed ones of these services). */
const RUN_BY_ROLL = ["postgresql", "s3", "roll_web_crawler"];

type Connector = {
  id: string;
  name?: string;
  service_type?: string;
  index_name?: string;
  is_native?: boolean;
  status?: string;
  error?: string | null;
  last_sync_status?: string | null;
  last_sync_error?: string | null;
  last_synced?: string | null;
  last_indexed_document_count?: number;
};

type SyncJob = {
  id: string;
  status: string;
  job_type: string;
  connector: { id: string };
  indexed_document_count: number;
  deleted_document_count: number;
  total_document_count: number;
  created_at: string;
  completed_at?: string;
  error?: string;
};

const ACTIVE = ["pending", "in_progress", "canceling"];

/** A connector with its newest job. */
type Row = { connector: Connector; job: SyncJob | undefined };

const seconds = (iso: string | null | undefined) => (iso ? Math.floor(Date.parse(iso) / 1000) : null);

const CREATE_EXAMPLE = `PUT _connector/my-database
{
  "index_name": "content-my-database",
  "is_native": true,
  "service_type": "postgresql",
  "name": "My database"
}`;

/** The cluster's connectors, their last sync and "Sync now". */
export function ConnectorsTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const load = useCallback(async (): Promise<Row[]> => {
    const [cs, jobs] = await Promise.all([
      esJson<{ results: Connector[] }>(cluster.id, "GET", "/_connector?size=1000"),
      esJson<{ results: SyncJob[] }>(cluster.id, "GET", "/_connector/_sync_job?size=1000"),
    ]);
    // Jobs come newest first: the first one of each connector is its newest.
    const newest = new Map<string, SyncJob>();
    for (const j of jobs.results) if (!newest.has(j.connector.id)) newest.set(j.connector.id, j);
    return cs.results.map((c) => ({ connector: c, job: newest.get(c.id) }));
  }, [cluster.id]);
  const { value: rows, error, reload } = useLoad(load);
  const [actionError, setActionError] = useState<string | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const syncing = rows?.some((r) => r.job && ACTIVE.includes(r.job.status)) ?? false;

  // Poll while a sync runs, so its counts move.
  useEffect(() => {
    if (!syncing) return;
    const t = setInterval(reload, POLL_MS);
    return () => clearInterval(t);
  }, [syncing, reload]);

  const act = async (id: string, f: () => Promise<unknown>) => {
    setBusy(id);
    setActionError(null);
    try {
      await f();
      reload();
    } catch (e) {
      setActionError(errText(e));
    } finally {
      setBusy(null);
    }
  };
  const syncNow = (c: Connector) => act(c.id, () => esJson(cluster.id, "POST", "/_connector/_sync_job", { id: c.id, job_type: "full", trigger_method: "on_demand" }));
  const cancel = (j: SyncJob) => act(j.connector.id, () => esJson(cluster.id, "PUT", `/_connector/_sync_job/${encodeURIComponent(j.id)}/_cancel`));
  const createButton = (
    <button onClick={() => onOpenConsole(CREATE_EXAMPLE)} className={btn}>
      <Plus className="size-3.5" /> New connector
    </button>
  );

  return (
    <>
      <ErrorNote error={actionError} />
      <Panel title="Connectors" aside={rows && rows.length > 0 && createButton}>
        {error ? (
          <LoadError error={error} onRetry={reload} />
        ) : !rows ? (
          <p className="px-5 py-4 text-[13px] text-subtle">
            <span className="shimmer">Loading connectors…</span>
          </p>
        ) : rows.length === 0 ? (
          <Empty title="No connectors yet" action={createButton}>
            A connector keeps an index in step with a PostgreSQL database or an S3 bucket. roll runs the syncs of Elastic-managed
            (<span className="font-mono text-fg">is_native</span>) <span className="font-mono text-fg">postgresql</span> and{" "}
            <span className="font-mono text-fg">s3</span> connectors, on demand or on their schedule.
          </Empty>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full min-w-[760px] text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[12px] text-subtle">
                  <th className="px-5 py-2 font-normal">Connector</th>
                  <th className="px-3 py-2 font-normal">Index</th>
                  <th className="px-3 py-2 font-normal">Status</th>
                  <th className="px-3 py-2 font-normal">Last sync</th>
                  <th className="px-3 py-2 text-right font-normal">Documents</th>
                  <th className="px-5 py-2" />
                </tr>
              </thead>
              <tbody>
                {rows.map(({ connector: c, job }) => {
                  const runs = c.is_native === true && RUN_BY_ROLL.includes(c.service_type ?? "");
                  const active = job && ACTIVE.includes(job.status);
                  const synced = seconds(c.last_synced);
                  const problem = c.last_sync_error ?? c.error;
                  return (
                    <tr key={c.id} className="border-b border-line align-top last:border-0">
                      <td className="px-5 py-2.5">
                        <p className="font-medium">{c.name || c.id}</p>
                        <p className="font-mono text-[11px] text-subtle">
                          {c.service_type ?? "no service"}
                          {!runs && " · self-managed"}
                        </p>
                      </td>
                      <td className="px-3 py-2.5 font-mono text-[12px]">{c.index_name ?? "—"}</td>
                      <td className="px-3 py-2.5">
                        <span className="text-[12px] text-muted">{(c.status ?? "created").replace(/_/g, " ")}</span>
                        {problem && <p className="mt-0.5 max-w-xs text-[12px] text-danger">{problem}</p>}
                      </td>
                      <td className="px-3 py-2.5 text-[12px]">
                        {active ? (
                          <span className="text-accent">
                            <span className="shimmer">{job.status === "canceling" ? "Canceling…" : job.status === "pending" ? "Waiting…" : "Syncing…"}</span>
                          </span>
                        ) : (
                          <span className="text-muted">
                            {c.last_sync_status ?? "never"}
                            {synced != null && <span className="text-subtle"> · {ago(synced)}</span>}
                          </span>
                        )}
                        {job && (
                          <p className="font-mono text-[11px] text-subtle">
                            {job.job_type} · +{formatCount(job.indexed_document_count)} −{formatCount(job.deleted_document_count)}
                          </p>
                        )}
                      </td>
                      <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">
                        {job && job.status === "completed" ? formatCount(job.total_document_count) : formatCount(c.last_indexed_document_count ?? null)}
                      </td>
                      <td className="px-5 py-2.5 text-right">
                        {active && job.status !== "canceling" ? (
                          <button onClick={() => cancel(job)} disabled={busy === c.id} className={btn}>
                            <Square className="size-3.5" /> Cancel
                          </button>
                        ) : (
                          runs && (
                            <button onClick={() => syncNow(c)} disabled={busy === c.id || !!active || c.status === "needs_configuration"} className={btn}>
                              <Play className="size-3.5" /> Sync now
                            </button>
                          )
                        )}
                      </td>
                    </tr>
                  );
                })}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </>
  );
}
