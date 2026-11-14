import { useCallback, useEffect, useState } from "react";
import { formatBytes } from "@/lib/roll-api";
import { esJson, formatCount, type SearchCluster, type SearchIndex } from "@/lib/search-api";
import { ErrorNote, input, Panel } from "../ui";

/** A data stream as `GET _data_stream` lists it (the fields the panel reads). */
interface DataStream {
  name: string;
  generation: number;
  indices: { index_name: string }[];
  template?: string;
  ilm_policy?: string;
  next_generation_managed_by?: string;
  lifecycle?: { enabled?: boolean; data_retention?: string; effective_retention?: string };
  hidden?: boolean;
  system?: boolean;
}

const RETENTION = [
  { label: "Kept forever", value: "" },
  { label: "1 day", value: "1d" },
  { label: "7 days", value: "7d" },
  { label: "30 days", value: "30d" },
  { label: "90 days", value: "90d" },
  { label: "1 year", value: "365d" },
];

/**
 * A cluster's data streams (logs, metrics, traces written to `logs-*`-style names): their backing
 * indices, documents and size, and on Elasticsearch-flavor clusters the data stream lifecycle's
 * retention, set in place (`PUT _data_stream/<name>/_lifecycle`). Shown only when there are any.
 */
export function DataStreams({ cluster, indices, onSelect }: { cluster: SearchCluster; indices: SearchIndex[]; onSelect: (index: string) => void }) {
  const [streams, setStreams] = useState<DataStream[] | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const opensearch = cluster.flavor === "opensearch";

  const load = useCallback(async () => {
    try {
      const r = await esJson<{ data_streams: DataStream[] }>(cluster.id, "GET", "/_data_stream");
      setStreams(r.data_streams.filter((d) => !d.system));
    } catch {
      setStreams([]);
    }
  }, [cluster.id]);

  // Listed with the indices (it reloads when they do).
  useEffect(() => {
    load();
  }, [load, indices]);

  if (!streams || streams.length === 0) return null;
  const byName = new Map(indices.map((i) => [i.index, i]));

  const setRetention = async (name: string, value: string) => {
    setBusy(name);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `/_data_stream/${encodeURIComponent(name)}/_lifecycle`, value ? { data_retention: value } : {});
      await load();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    } finally {
      setBusy(null);
    }
  };

  return (
    <Panel title="Data streams">
      <div className="overflow-x-auto">
        <table className="w-full text-[13px]">
          <thead>
            <tr className="border-b border-line text-left text-[11px] text-subtle">
              <th className="px-5 py-2 font-normal">Data stream</th>
              <th className="px-3 py-2 text-right font-normal">Indices</th>
              <th className="px-3 py-2 text-right font-normal">Docs</th>
              <th className="px-3 py-2 text-right font-normal">Size</th>
              <th className="px-5 py-2 font-normal">Retention</th>
            </tr>
          </thead>
          <tbody>
            {streams.map((d) => {
              const backing = d.indices.map((i) => byName.get(i.index_name)).filter((i): i is SearchIndex => !!i);
              const docs = backing.reduce((n, i) => n + (i.docs ?? 0), 0);
              const size = backing.reduce((n, i) => n + (i.size_bytes ?? 0), 0);
              const write = d.indices[d.indices.length - 1]?.index_name;
              const ilm = d.next_generation_managed_by === "Index Lifecycle Management" && d.ilm_policy;
              const retention = d.lifecycle?.data_retention ?? "";
              const options = RETENTION.some((o) => o.value === retention) ? RETENTION : [...RETENTION, { label: retention, value: retention }];
              return (
                <tr key={d.name} className="border-b border-line last:border-0">
                  <td className="max-w-80 px-5 py-2.5">
                    <div className="truncate font-mono text-[12px]">{d.name}</div>
                    {d.template && <div className="truncate font-mono text-[11px] text-subtle">template {d.template}</div>}
                  </td>
                  <td className="whitespace-nowrap px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">
                    {write ? (
                      <button onClick={() => onSelect(write)} className="hover:underline" title={`Browse the write index, ${write}`}>
                        {d.indices.length}
                      </button>
                    ) : (
                      d.indices.length
                    )}
                  </td>
                  <td className="whitespace-nowrap px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatCount(docs)}</td>
                  <td className="whitespace-nowrap px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatBytes(size)}</td>
                  <td className="px-5 py-2.5 text-[12px] text-muted">
                    {ilm ? (
                      <span>
                        ILM policy <span className="font-mono">{d.ilm_policy}</span>
                      </span>
                    ) : opensearch ? (
                      <span className="text-subtle">ISM policies (console)</span>
                    ) : (
                      <span className="flex items-center gap-2">
                        <select value={retention} disabled={busy !== null} onChange={(e) => setRetention(d.name, e.target.value)} aria-label={`Retention of ${d.name}`} className={input}>
                          {options.map((o) => (
                            <option key={o.value} value={o.value}>
                              {o.label}
                            </option>
                          ))}
                        </select>
                        {busy === d.name && <span className="shimmer">Saving…</span>}
                        {!busy && d.lifecycle?.effective_retention && d.lifecycle.effective_retention !== retention && (
                          <span className="font-mono text-subtle" title="The cluster's global retention applies">
                            effective {d.lifecycle.effective_retention}
                          </span>
                        )}
                      </span>
                    )}
                  </td>
                </tr>
              );
            })}
          </tbody>
        </table>
      </div>
      {error && (
        <div className="px-5 pb-3">
          <ErrorNote error={error} />
        </div>
      )}
      {!opensearch && (
        <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
          The data stream lifecycle rolls a stream over to a new backing index as it grows and deletes backing indices whose
          documents are all past the retention, so a stream&apos;s storage stops growing.
        </p>
      )}
    </Panel>
  );
}
