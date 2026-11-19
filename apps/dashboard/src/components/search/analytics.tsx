/**
 * The Relevance tab's "Search analytics" (Algolia's and Elastic App Search's analytics): what an
 * index is searched for, the searches that find nothing and the queries whose results nobody
 * clicks, each with the action that answers it: a synonym or a query rule, prefilled in the
 * Relevance tab's editors.
 *
 * Analytics are off until the owner turns them on for an index (`index.roll.analytics.enabled`):
 * query text is the tenant's data. Turning them off deletes what's kept.
 *
 * The chart follows the Performance tab's: neutral ink for searches, the danger token for the
 * searches with no results, a hairline grid and a value on hover or focus. Nothing animates.
 */

import { BookOpen, Plus, Search } from "lucide-react";
import { useCallback, useEffect, useState } from "react";
import {
  ANALYTICS_RANGES,
  ANALYTICS_SETTING,
  emptyText,
  enabledIndices,
  fillPoints,
  percent,
  type AnalyticsList,
  type AnalyticsRange,
  type Prefill,
  type QueryRow,
  type SearchAnalytics,
} from "@/lib/search-analytics";
import { esJson, searchApi, type SearchCluster } from "@/lib/search-api";
import { Segmented } from "../redis/shared";
import { btn, Confirm, input, Panel } from "../ui";
import { errText } from "./relevance-shared";

const enc = encodeURIComponent;
const W = 300;
const H = 56;

const count = (v: number) => (v >= 10_000 ? `${(v / 1000).toFixed(0)}k` : v >= 1000 ? `${(v / 1000).toFixed(1)}k` : String(v));

function when(at: number, step: number) {
  const d = new Date(at * 1000);
  const day = d.toLocaleDateString("en-US", { month: "short", day: "numeric" });
  return step >= 86_400 ? day : `${day} ${d.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit", hour12: false })}`;
}

/** Searches per hour (or day) as bars, the ones that found nothing over them. */
function Volume({ a }: { a: SearchAnalytics }) {
  const pts = fillPoints(a);
  const [hover, setHover] = useState<number | null>(null);
  const n = pts.length;
  const top = Math.max(1, ...pts.map((p) => p.searches));
  const bw = W / Math.max(n, 1);
  const h = (v: number) => (v / top) * (H - 4);
  const p = hover !== null ? pts[hover] : null;
  return (
    <div className="px-5 pt-4">
      <div className="flex items-baseline justify-between gap-2 text-[12px]">
        <span className="text-muted">
          Searches per {a.step_s >= 86_400 ? "day" : "hour"}
          <i className="ml-3 inline-block size-2 rounded-[3px] bg-danger align-middle" aria-hidden />
          <span className="ml-1.5 text-subtle">no results</span>
        </span>
        <span className="font-mono text-[11px] tabular-nums text-subtle">
          {p ? `${when(p.at, a.step_s)} · ${p.searches.toLocaleString("en-US")} searches · ${p.no_hits.toLocaleString("en-US")} empty` : `peak ${count(top)}`}
        </span>
      </div>
      <div
        className="relative mt-2 text-fg outline-none"
        tabIndex={0}
        role="img"
        aria-label={`Searches per ${a.step_s >= 86_400 ? "day" : "hour"}: ${a.totals.searches} in all, ${a.totals.no_hits} with no results`}
        onPointerMove={(e) => {
          const r = e.currentTarget.getBoundingClientRect();
          setHover(Math.max(0, Math.min(n - 1, Math.floor(((e.clientX - r.left) / r.width) * n))));
        }}
        onPointerLeave={() => setHover(null)}
        onBlur={() => setHover(null)}
        onKeyDown={(e) => {
          if (e.key === "ArrowLeft") setHover((x) => Math.max(0, (x ?? n) - 1));
          if (e.key === "ArrowRight") setHover((x) => Math.min(n - 1, (x ?? -1) + 1));
        }}
      >
        <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="block h-14 w-full">
          <line x1={0} x2={W} y1={H - 0.5} y2={H - 0.5} stroke="var(--line-2)" vectorEffect="non-scaling-stroke" />
          {pts.map((pt, i) => (
            <g key={pt.at} opacity={hover === null || hover === i ? 1 : 0.55}>
              <rect x={i * bw + bw * 0.15} width={bw * 0.7} y={H - h(pt.searches)} height={h(pt.searches)} fill="currentColor" fillOpacity={0.35} />
              <rect x={i * bw + bw * 0.15} width={bw * 0.7} y={H - h(pt.no_hits)} height={h(pt.no_hits)} fill="var(--danger)" />
            </g>
          ))}
        </svg>
      </div>
    </div>
  );
}

function Stat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <div className="text-[11px] text-subtle">{label}</div>
      <div className="font-mono text-[17px] font-semibold tracking-[-0.03em] tabular-nums">{value}</div>
    </div>
  );
}

function Rows({ list, rows, a, onAct }: { list: AnalyticsList; rows: QueryRow[]; a: SearchAnalytics; onAct: (p: Prefill) => void }) {
  if (rows.length === 0) return <p className="px-5 py-6 text-[13px] text-muted">{emptyText(list, a)}</p>;
  const clicks = a.totals.clicks > 0;
  return (
    // `relative`: the screen-reader-only header is positioned, and would widen the page.
    <div className="relative overflow-x-auto">
      <table className="w-full min-w-[50rem] table-fixed text-[13px]">
        <colgroup>
          <col />
          <col className="w-24" />
          <col className="w-24" />
          <col className="w-28" />
          {clicks && <col className="w-28" />}
          <col className="w-48" />
        </colgroup>
        <thead>
          <tr className="border-b border-line text-left text-[11px] text-subtle">
            <th className="px-5 py-2 font-normal">Query</th>
            <th className="px-2 py-2 text-right font-normal">Searches</th>
            <th className="px-2 py-2 text-right font-normal">No results</th>
            <th className="px-2 py-2 text-right font-normal">{clicks ? "Click-through" : "Avg hits"}</th>
            {clicks && <th className="px-2 py-2 text-right font-normal">Avg position</th>}
            <th className="px-5 py-2 text-right font-normal">
              <span className="sr-only">Actions</span>
            </th>
          </tr>
        </thead>
        <tbody>
          {rows.map((r) => (
            <tr key={r.query} className="group border-b border-line last:border-0">
              <td className="truncate px-5 py-2 font-mono text-[12px]" title={r.query}>
                {r.query}
              </td>
              <td className="px-2 py-2 text-right font-mono text-[12px] tabular-nums">{r.searches.toLocaleString("en-US")}</td>
              <td className={`px-2 py-2 text-right font-mono text-[12px] tabular-nums ${r.no_hits > 0 ? "text-fg" : "text-subtle"}`}>{percent(r.searches ? r.no_hits / r.searches : null)}</td>
              <td className="px-2 py-2 text-right font-mono text-[12px] tabular-nums text-muted">{clicks ? percent(r.ctr) : (r.avg_hits?.toLocaleString("en-US") ?? "—")}</td>
              {clicks && <td className="px-2 py-2 text-right font-mono text-[12px] tabular-nums text-muted">{r.avg_position ?? "—"}</td>}
              <td className="whitespace-nowrap px-5 py-1.5 text-right">
                <button onClick={() => onAct({ kind: "synonym", query: r.query })} className="rounded-md px-2 py-1 text-[12px] text-muted transition hover:bg-panel-2 hover:text-fg" title={`Add a synonym for “${r.query}”`} aria-label={`Add a synonym for “${r.query}”`}>
                  <BookOpen className="mr-1 inline size-3.5 align-[-2px]" />
                  Synonym
                </button>
                <button onClick={() => onAct({ kind: "rule", query: r.query })} className="rounded-md px-2 py-1 text-[12px] text-muted transition hover:bg-panel-2 hover:text-fg" title={`Add a query rule for “${r.query}”`} aria-label={`Add a query rule for “${r.query}”`}>
                  <Plus className="mr-1 inline size-3.5 align-[-2px]" />
                  Rule
                </button>
              </td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}

/** Which indices have analytics on, and turning it on or off for one. */
function Switches({ cluster, indices, enabled, onChanged }: { cluster: SearchCluster; indices: string[]; enabled: string[]; onChanged: () => void }) {
  const [pick, setPick] = useState("");
  const [off, setOff] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const candidates = indices.filter((i) => !enabled.includes(i));
  const set = async (index: string, on: boolean) => {
    setBusy(true);
    setError(null);
    try {
      await esJson(cluster.id, "PUT", `/${enc(index)}/_settings`, { [ANALYTICS_SETTING]: on ? true : null });
      setOff(null);
      setPick("");
      onChanged();
    } catch (e) {
      setError(errText(e));
    } finally {
      setBusy(false);
    }
  };
  return (
    <div className="space-y-3 border-t border-line px-5 py-4">
      <div className="flex flex-wrap items-center gap-2 text-[12px] text-muted">
        <span>{enabled.length === 0 ? "Not recording searches of any index." : enabled.length === 1 ? "Recording searches of" : "Recording searches of these indices:"}</span>
        {enabled.map((i) => (
          <span key={i} className="inline-flex items-center gap-2 rounded-md border border-line py-0.5 pl-2 pr-1 text-[11px]">
            <span className="font-mono text-fg">{i}</span>
            <button onClick={() => setOff(i)} className="rounded px-1 text-subtle transition hover:text-danger" aria-label={`Turn analytics off for ${i}`}>
              Turn off
            </button>
          </span>
        ))}
        {candidates.length > 0 && (
          <span className="ml-auto inline-flex items-center gap-2">
            <select className={`${input} py-1 font-mono text-[12px]`} value={pick} onChange={(e) => setPick(e.target.value)} aria-label="Index to record">
              <option value="">Choose an index…</option>
              {candidates.map((i) => (
                <option key={i} value={i}>
                  {i}
                </option>
              ))}
            </select>
            <button className={btn} disabled={!pick || busy} onClick={() => set(pick, true)}>
              {busy && !off ? <span className="shimmer">Turning on…</span> : "Turn on"}
            </button>
          </span>
        )}
      </div>
      {off && (
        <Confirm word={off} action="Turn off and delete" busy={busy} onConfirm={() => set(off, false)} onCancel={() => setOff(null)}>
          Stops recording searches of <span className="font-mono text-fg">{off}</span> and deletes every query kept for it.
        </Confirm>
      )}
      {error && <p className="text-[13px] text-danger">{error}</p>}
      <p className="text-[12px] text-subtle">
        Only the query text, counted per hour, is kept, for 30 days; never a header, a credential or a document. Clients report clicks with{" "}
        <span className="font-mono">search_click</span> events (<span className="break-all font-mono">POST _application/analytics/&lt;collection&gt;/event/search_click</span>).
      </p>
    </div>
  );
}

export function SearchAnalyticsPanel({ cluster, onAct }: { cluster: SearchCluster; onAct: (p: Prefill) => void }) {
  const [range, setRange] = useState<AnalyticsRange>("7d");
  const [index, setIndex] = useState<string>("");
  const [list, setList] = useState<AnalyticsList>("top");
  const [data, setData] = useState<SearchAnalytics | null>(null);
  const [indices, setIndices] = useState<string[] | null>(null);
  const [enabled, setEnabled] = useState<string[]>([]);
  const [error, setError] = useState<string | null>(null);
  const [tick, setTick] = useState(0);
  const reload = useCallback(() => setTick((t) => t + 1), []);

  useEffect(() => {
    let alive = true;
    setError(null);
    Promise.all([
      searchApi.analytics(cluster.id, index || null, range),
      searchApi.indices(cluster.id),
      esJson<Record<string, { settings?: Record<string, unknown> }>>(cluster.id, "GET", `/_settings/${ANALYTICS_SETTING}?flat_settings=true`),
    ]).then(
      ([a, idx, settings]) => {
        if (!alive) return;
        setData(a);
        setIndices(idx.map((i) => i.index).filter((i) => !i.startsWith(".")).sort());
        setEnabled(enabledIndices(settings));
      },
      (e) => alive && setError(errText(e)),
    );
    return () => {
      alive = false;
    };
  }, [cluster.id, index, range, tick]);

  const pickers = (
    <div className="flex items-center gap-2">
      <select className={`${input} py-1 font-mono text-[12px]`} value={index} onChange={(e) => setIndex(e.target.value)} aria-label="Index">
        <option value="">All indices</option>
        {[...new Set([...enabled, ...(data?.indices ?? [])])].sort().map((i) => (
          <option key={i} value={i}>
            {i}
          </option>
        ))}
      </select>
      <Segmented value={range} options={ANALYTICS_RANGES.map((r) => ({ label: r, value: r }))} onChange={setRange} />
    </div>
  );

  if (error)
    return (
      <Panel title="Search analytics">
        <div className="flex flex-wrap items-center justify-between gap-3 px-5 py-4">
          <p className="text-[13px] text-danger">{error}</p>
          <button onClick={reload} className={btn}>
            Retry
          </button>
        </div>
      </Panel>
    );
  if (!data || !indices)
    return (
      <Panel title="Search analytics">
        <p className="px-5 py-4 text-[13px] text-subtle">
          <span className="shimmer">Loading search analytics…</span>
        </p>
      </Panel>
    );
  if (enabled.length === 0 && data.totals.searches === 0)
    return (
      <Panel title="Search analytics">
        <div className="flex flex-col items-start gap-2 px-5 pt-8 pb-2">
          <p className="flex items-center gap-2 text-[14px] font-medium">
            <Search className="size-4 text-subtle" /> See what people search for
          </p>
          <p className="max-w-xl text-[13px] text-muted">
            Turn analytics on for an index to see its top queries, the searches that find nothing and the results nobody clicks, then answer
            them with a synonym or a query rule. It&apos;s off until you turn it on, because query text is your users&apos; data.
          </p>
        </div>
        {indices.length === 0 ? <p className="px-5 pb-6 text-[13px] text-subtle">Create an index first.</p> : <Switches cluster={cluster} indices={indices} enabled={enabled} onChanged={reload} />}
      </Panel>
    );
  const rows = data[list];
  return (
    <Panel title="Search analytics" aside={pickers}>
      <div className="grid grid-cols-2 gap-4 px-5 pt-4 sm:grid-cols-4">
        <Stat label="Searches" value={data.totals.searches.toLocaleString("en-US")} />
        <Stat label="No results" value={percent(data.totals.no_hit_rate)} />
        <Stat label="Clicks" value={data.totals.clicks.toLocaleString("en-US")} />
        <Stat label="Click-through" value={data.totals.clicks ? percent(data.totals.ctr) : "—"} />
      </div>
      <Volume a={data} />
      <div className="px-5 pt-5 pb-2">
        <Segmented
          value={list}
          options={[
            { label: "top queries", value: "top" as const },
            { label: `no results${data.no_results.length ? ` · ${data.no_results.length}` : ""}`, value: "no_results" as const },
            { label: "low click-through", value: "low_ctr" as const },
          ]}
          onChange={setList}
        />
      </div>
      <Rows list={list} rows={rows} a={data} onAct={onAct} />
      <Switches cluster={cluster} indices={indices} enabled={enabled} onChanged={reload} />
    </Panel>
  );
}
