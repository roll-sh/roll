/**
 * A cluster's Performance tab (Elastic Cloud's Performance tab and its slow logs): request
 * rate, latency percentiles, errors and rejections over the last hour, day or 14 days, by kind
 * of request, and the search and indexing slow logs, newest first.
 *
 * The charts follow the apps metrics' small multiples (components/apps/metrics.tsx): one
 * measure per chart, neutral ink for a single series, the percentiles in `--viz-2…4` with a
 * legend carrying each value, hairline grid, a crosshair and value on hover or focus. Nothing
 * animates, so reduced motion needs nothing more.
 */

import { ChevronRight, Terminal } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { searchApi, type SearchCluster } from "@/lib/search-api";
import {
  consoleText,
  formatLatency,
  formatRate,
  linePath,
  METRICS_RANGES,
  prettySource,
  REQUEST_KINDS,
  series as toSeries,
  slowEntry,
  sourceCut,
  type MetricsRange,
  type RequestKind,
  type SearchMetrics,
  type SearchSlowLog,
  type SlowEntry,
} from "@/lib/search-performance";
import { usePageVisible } from "../apps/live";
import { Segmented } from "../redis/shared";
import { btn, CopyButton, panel, useCopy } from "../ui";

const W = 300;
const H = 84;

interface Line {
  label: string;
  values: (number | null)[];
  color: string;
}

/** A round top for the axis: 1, 2, 2.5, 5 × 10ⁿ. */
function niceMax(v: number) {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  return ([1, 2, 2.5, 5, 10].find((m) => m * p >= v) ?? 10) * p;
}

function when(at: number, long: boolean) {
  const d = new Date(at * 1000);
  const time = d.toLocaleTimeString("en-US", { hour: "2-digit", minute: "2-digit", hour12: false });
  return long ? `${d.toLocaleDateString("en-US", { month: "short", day: "numeric" })} ${time}` : time;
}

function Chart({ title, at, lines, format, headline, long }: { title: string; at: number[]; lines: Line[]; format: (v: number) => string; headline: string[]; long: boolean }) {
  const [hover, setHover] = useState<number | null>(null);
  const n = at.length;
  const top = niceMax(Math.max(0, ...lines.flatMap((l) => l.values.map((v) => v ?? 0))));
  const x = (i: number) => (n <= 1 ? W : (i / (n - 1)) * W);
  const y = (v: number) => H - 2 - (v / top) * (H - 16);
  const pick = (clientX: number, rect: DOMRect) => {
    if (!n) return;
    setHover(Math.max(0, Math.min(n - 1, Math.round(((clientX - rect.left) / rect.width) * (n - 1)))));
  };
  return (
    <figure className={`${panel} p-4`}>
      <figcaption className="flex items-baseline justify-between gap-2">
        <span className="text-[12px] text-muted">{title}</span>
        <span className="font-mono text-[11px] text-subtle tabular-nums">{hover !== null ? when(at[hover], long) : ""}</span>
      </figcaption>
      <div className="mt-1 flex flex-wrap items-baseline gap-x-3">
        {lines.map((l, i) => (
          <span key={l.label} className="inline-flex items-baseline gap-1.5">
            {lines.length > 1 && <i className="inline-block size-2 rounded-[3px]" style={{ background: l.color }} aria-hidden />}
            <span className="font-mono text-[17px] font-semibold tracking-[-0.03em] tabular-nums">{hover !== null ? (l.values[hover] == null ? "—" : format(l.values[hover] as number)) : headline[i]}</span>
            {lines.length > 1 && <span className="text-[11px] text-subtle">{l.label}</span>}
          </span>
        ))}
      </div>
      <div
        className="relative mt-2 text-fg outline-none"
        tabIndex={0}
        role="img"
        aria-label={`${title}: ${lines.map((l, i) => `${l.label} ${headline[i]}`).join(", ")}`}
        onPointerMove={(e) => pick(e.clientX, e.currentTarget.getBoundingClientRect())}
        onPointerLeave={() => setHover(null)}
        onBlur={() => setHover(null)}
        onKeyDown={(e) => {
          if (e.key === "ArrowLeft") setHover((h) => Math.max(0, (h ?? n - 1) - 1));
          if (e.key === "ArrowRight") setHover((h) => Math.min(n - 1, (h ?? n - 1) + 1));
        }}
      >
        <svg viewBox={`0 0 ${W} ${H}`} preserveAspectRatio="none" className="block h-[84px] w-full overflow-visible">
          <line x1={0} x2={W} y1={y(top)} y2={y(top)} stroke="var(--line)" vectorEffect="non-scaling-stroke" />
          <line x1={0} x2={W} y1={y(top / 2)} y2={y(top / 2)} stroke="var(--line)" vectorEffect="non-scaling-stroke" />
          <line x1={0} x2={W} y1={y(0)} y2={y(0)} stroke="var(--line-2)" vectorEffect="non-scaling-stroke" />
          {lines.map((l) => (
            <path key={l.label} d={linePath(l.values, x, y)} fill="none" stroke={l.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
          ))}
          {hover !== null && <line x1={x(hover)} x2={x(hover)} y1={0} y2={H} stroke="var(--line-3)" vectorEffect="non-scaling-stroke" />}
        </svg>
        <span className="pointer-events-none absolute top-0 left-0 -translate-y-0.5 font-mono text-[10px] leading-none text-subtle">{format(top)}</span>
      </div>
    </figure>
  );
}

const count = (v: number) => (v >= 10_000 ? `${(v / 1000).toFixed(0)}k` : v >= 1000 ? `${(v / 1000).toFixed(1)}k` : String(Math.round(v)));
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

export function PerformanceTab({ cluster, onOpenConsole }: { cluster: SearchCluster; onOpenConsole: (text: string) => void }) {
  const [range, setRange] = useState<MetricsRange>("1h");
  const [kind, setKind] = useState<RequestKind>("all");
  const [metrics, setMetrics] = useState<SearchMetrics | null>(null);
  const [slow, setSlow] = useState<SearchSlowLog | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const [slowErr, setSlowErr] = useState<string | null>(null);
  const visible = usePageVisible();

  useEffect(() => {
    if (!visible) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      const [m, s] = await Promise.allSettled([searchApi.metrics(cluster.id, range), searchApi.slowlog(cluster.id)]);
      if (!alive) return;
      if (m.status === "fulfilled") {
        setMetrics(m.value);
        setErr(null);
      } else setErr(message(m.reason));
      if (s.status === "fulfilled") {
        setSlow(s.value);
        setSlowErr(null);
      } else setSlowErr(message(s.reason));
      // A day or two weeks change slowly: looked at less often.
      timer = setTimeout(load, range === "1h" ? 30_000 : 120_000);
    };
    void load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [cluster.id, range, visible]);

  const m = metrics?.range === range ? metrics : null;
  const s = useMemo(() => (m ? toSeries(m, kind) : null), [m, kind]);
  const t = m?.totals[kind];
  const long = range !== "1h";
  const per = m ? (m.step_s === 60 ? "per minute" : m.step_s % 3600 === 0 ? `per ${m.step_s / 3600} h` : `per ${m.step_s / 60} min`) : "";
  const sum = (a: number[]) => a.reduce((x, v) => x + v, 0);

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="max-w-xl text-[13px] text-muted">
          Requests the cluster answered, from the node that serves it: latency counts the wait for a slot and making writes durable. An asleep cluster records nothing.
        </p>
        <div className="flex items-center gap-2">
          <div className="w-56">
            <Segmented value={kind} options={REQUEST_KINDS} onChange={setKind} />
          </div>
          <div className="w-44">
            <Segmented value={range} options={METRICS_RANGES.map((r) => ({ label: r.value, value: r.value }))} onChange={setRange} />
          </div>
        </div>
      </div>

      {err && !m && <p className="text-[13px] text-danger">Metrics didn&apos;t load: {err}</p>}
      {!err && !m && (
        <p className="text-[13px] text-subtle">
          <span className="shimmer">Loading metrics…</span>
        </p>
      )}
      {m && s && t && (
        <>
          {t.requests === 0 ? (
            <div className={`${panel} px-5 py-6`}>
              <p className="text-[13px]">No {kind === "all" ? "" : `${kind} `}requests in the last {METRICS_RANGES.find((r) => r.value === range)?.label}.</p>
              <p className="mt-1 text-[12px] text-subtle">Charts fill in a minute after the cluster serves its first request.</p>
            </div>
          ) : (
            <div className="grid gap-3 sm:grid-cols-2">
              <Chart title={`Requests / s, ${per}`} at={s.at} long={long} format={formatRate} headline={[`${formatRate(t.rate)} avg`]} lines={[{ label: "requests", values: s.rate, color: "var(--soft)" }]} />
              <Chart
                title="Latency"
                at={s.at}
                long={long}
                format={formatLatency}
                headline={[formatLatency(t.p50_ms), formatLatency(t.p95_ms), formatLatency(t.p99_ms)]}
                lines={[
                  { label: "p50", values: s.p50, color: "var(--viz-2)" },
                  { label: "p95", values: s.p95, color: "var(--viz-3)" },
                  { label: "p99", values: s.p99, color: "var(--viz-4)" },
                ]}
              />
              <Chart
                title={`Errors, ${per}`}
                at={s.at}
                long={long}
                format={count}
                headline={[count(sum(s.client)), count(sum(s.server))]}
                lines={[
                  { label: "4xx", values: s.client, color: "var(--viz-2)" },
                  { label: "5xx", values: s.server, color: "var(--viz-4)" },
                ]}
              />
              <Chart title={`Rejections (429), ${per}`} at={s.at} long={long} format={count} headline={[count(t.rejected)]} lines={[{ label: "429", values: s.rejected, color: "var(--soft)" }]} />
            </div>
          )}
          {err && <p className="text-[12px] text-danger">Couldn&apos;t refresh: {err}</p>}
        </>
      )}

      <SlowLog log={slow} error={slowErr} onOpenConsole={onOpenConsole} />
    </div>
  );
}

/** Rows the slow log shows at first, and adds per "Show more". */
const PAGE = 50;

const SETUP = `PUT /my-index/_settings
{
  "index.search.slowlog.threshold.query.warn": "500ms",
  "index.search.slowlog.threshold.fetch.warn": "200ms",
  "index.indexing.slowlog.threshold.index.warn": "200ms"
}`;

function SlowLog({ log, error, onOpenConsole }: { log: SearchSlowLog | null; error: string | null; onOpenConsole: (text: string) => void }) {
  const entries = useMemo(() => (log ? log.entries.map(slowEntry) : []), [log]);
  const [open, setOpen] = useState<number | null>(null);
  const [shown, setShown] = useState(PAGE);
  const { copied, copy } = useCopy();
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Slow log">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Slow log <span className="text-subtle">· searches and writes past their index&apos;s thresholds, newest first; the last {log?.max_entries ?? 500} kept</span>
        </p>
      </div>
      {error && !log ? (
        <p className="px-4 py-3 text-[12.5px] text-danger">The slow log didn&apos;t load: {error}</p>
      ) : !log ? (
        <p className="px-4 py-3 text-[12.5px] text-subtle">
          <span className="shimmer">Reading the slow log…</span>
        </p>
      ) : entries.length === 0 ? (
        <div className="space-y-2 px-4 py-4 text-[12.5px]">
          <p>Nothing slow yet.</p>
          <p className="text-subtle">
            Thresholds are index settings, off until set: <span className="font-mono">index.search.slowlog.threshold.query|fetch.*</span> and{" "}
            <span className="font-mono">index.indexing.slowlog.threshold.index.*</span> (warn, info, debug, trace).
          </p>
          <button className={btn} onClick={() => onOpenConsole(SETUP)}>
            <Terminal className="size-3.5" /> Set thresholds in the console
          </button>
        </div>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-[12px]">
            <thead className="text-left text-subtle">
              <tr>
                {["", "time", "level", "phase", "index", "took", "hits / id", "x-opaque-id"].map((h, i) => (
                  <th key={i} className={`px-3 py-1.5 font-normal ${h === "took" ? "text-right" : ""}`}>
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody className="font-mono text-[11.5px] tabular-nums">
              {entries.slice(0, shown).map((e, i) => (
                <SlowRow key={i} e={e} open={open === i} onToggle={() => setOpen(open === i ? null : i)} copied={copied} copy={copy} onOpenConsole={onOpenConsole} n={i} />
              ))}
            </tbody>
          </table>
          {entries.length > shown && (
            <div className="flex items-center justify-between gap-2 border-t border-line px-4 py-2 text-[12px] text-subtle">
              <span>
                {shown} of {entries.length}
              </span>
              <button className={btn} onClick={() => setShown((n) => n + PAGE)}>
                Show {Math.min(PAGE, entries.length - shown)} more
              </button>
            </div>
          )}
        </div>
      )}
      {error && log && <p className="border-t border-line px-4 py-2 text-[12px] text-danger">Couldn&apos;t refresh: {error}</p>}
    </section>
  );
}

function SlowRow({ e, n, open, onToggle, copied, copy, onOpenConsole }: { e: SlowEntry; n: number; open: boolean; onToggle: () => void; copied: string | null; copy: (k: string, t: string) => void; onOpenConsole: (text: string) => void }) {
  const cut = sourceCut(e);
  return (
    <>
      <tr className="border-t border-line hover:bg-panel-2">
        <td className="w-6 pl-2">
          <button type="button" onClick={onToggle} aria-expanded={open} aria-label={open ? "Hide source" : "Show source"} className="p-1 text-subtle hover:text-fg">
            <ChevronRight className={`size-3.5 transition-transform duration-150 motion-reduce:transition-none ${open ? "rotate-90" : ""}`} />
          </button>
        </td>
        <td className="px-3 py-1.5 whitespace-nowrap text-muted">{e.at ? new Date(e.at).toLocaleString("en-US", { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit", hour12: false }) : "—"}</td>
        <td className={`px-3 py-1.5 ${e.level === "WARN" ? "text-fg" : "text-muted"}`}>{e.level.toLowerCase()}</td>
        <td className="px-3 py-1.5 text-muted">{e.phase}</td>
        <td className="max-w-[16rem] truncate px-3 py-1.5" title={e.index}>
          {e.index}
          {e.shard != null && <span className="text-subtle">[{e.shard}]</span>}
        </td>
        <td className="px-3 py-1.5 text-right" title={e.took}>
          {e.took}
        </td>
        <td className="max-w-[10rem] truncate px-3 py-1.5 text-muted">{e.phase === "index" ? (e.id ?? "—") : (e.totalHits ?? "—")}</td>
        <td className="max-w-[12rem] truncate px-3 py-1.5 text-muted">{e.phase === "index" ? "—" : (e.id ?? "—")}</td>
      </tr>
      {open && (
        <tr className="border-t border-line bg-panel-2/50">
          <td colSpan={8} className="px-4 py-3">
            {e.source ? (
              <pre className="max-h-72 overflow-auto rounded-lg border border-line bg-panel p-3 text-[11.5px] leading-relaxed whitespace-pre-wrap break-all">{prettySource(e.source)}</pre>
            ) : (
              <p className="text-[12px] text-subtle">No source: the request had no body{e.phase === "index" ? ", or index.indexing.slowlog.source is 0" : ""}.</p>
            )}
            <div className="mt-2 flex flex-wrap items-center gap-2 font-sans">
              <button className={btn} onClick={() => onOpenConsole(consoleText(e))}>
                <Terminal className="size-3.5" /> Open in console
              </button>
              {e.source && (
                <span className="inline-flex items-center gap-1 text-[12px] text-subtle">
                  <CopyButton id={`slow-${n}`} text={e.source} copied={copied} onCopy={copy} /> copy source
                </span>
              )}
              {cut && <span className="text-[11.5px] text-subtle">Cut at its first 1,000 characters.</span>}
            </div>
          </td>
        </tr>
      )}
    </>
  );
}
