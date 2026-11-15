/**
 * The last hour, day or week as small multiples (a minute a point, or buckets of minutes past
 * 360 points: sums for counts, peaks for gauges and latencies), drawn by the dataviz skill's rules: one measure per
 * chart (one axis each, never two scales), single series in neutral ink with no legend, the
 * latency chart's three percentiles in chart tokens `--viz-2…4` (the three that pass the skill's
 * validator against each other in both modes, not just as neighbours: 1 and 3 are both green) with a legend that
 * carries each latest value, hairline grid, a crosshair and a value-first tooltip on hover or
 * focus, and the same numbers as a table. Each deployment that went live in the window is a
 * dashed rule on every chart (Vercel's and Render's deploy markers): did this deploy move the
 * errors or the latency.
 */

import { Table2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { appsApi, formatMs, type Deployment, type Metrics as MetricsData, type RouteStat, type RouteStats } from "@/lib/apps-api";
import { formatBytes } from "@/lib/roll-api";
import { btn, CopyButton, panel, useCopy } from "../ui";
import { usePageVisible } from "./live";
import { clock, message } from "./shared";

const W = 300;
const H = 84;

interface Series {
  label: string;
  values: number[];
  color: string;
}

const count = (n: number) => (n >= 10_000 ? `${(n / 1000).toFixed(0)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(Math.round(n * 100) / 100));
const cores = (n: number) => `${n.toFixed(n < 1 ? 2 : 1)}`;

/** A round top for the axis: 1, 2, 2.5, 5 × 10ⁿ. */
function niceMax(v: number) {
  if (v <= 0) return 1;
  const p = 10 ** Math.floor(Math.log10(v));
  return ([1, 2, 2.5, 5, 10].find((m) => m * p >= v) ?? 10) * p;
}

/** What a chart's headline says when nothing is hovered: the window's total (counts per minute) or its peak (gauges, latencies). */
type Agg = "total" | "peak";

const RANGES = [
  { minutes: 60, label: "hour", short: "1h" },
  { minutes: 24 * 60, label: "day", short: "24h" },
  { minutes: 7 * 24 * 60, label: "week", short: "7d" },
] as const;
type Range = (typeof RANGES)[number];

/** At most this many points a chart: a week of minutes in 28-minute buckets. */
const MAX_POINTS = 360;

const COUNTS = ["requests", "errors", "cold_starts", "egress_bytes"] as const;
const PEAKS = ["p50_ms", "p95_ms", "p99_ms", "cpu", "memory_bytes", "instances", "cold_start_ms"] as const;

/** `m` in buckets of `size` minutes (each named by its first): counts summed, the rest their peak. */
export function bucketed(m: MetricsData, size: number): MetricsData {
  if (size <= 1) return m;
  const out = { ...m, minutes: [] as number[] } as MetricsData;
  for (const k of [...COUNTS, ...PEAKS]) (out as unknown as Record<string, number[]>)[k] = [];
  for (let i = 0; i < m.minutes.length; i += size) {
    out.minutes.push(m.minutes[i]);
    for (const k of COUNTS) out[k].push((m[k] ?? []).slice(i, i + size).reduce((a, v) => a + (v ?? 0), 0));
    for (const k of PEAKS) out[k].push(Math.max(0, ...m[k].slice(i, i + size).map((v) => v ?? 0)));
  }
  return out;
}

/** A deployment going live, on the charts: `i` the point it fell in. */
export interface Mark {
  i: number;
  at: number;
  /** Its commit's first 7, or its id. */
  short: string;
  label: string;
}

/** The deployments that went live inside the window (`minutes`, points `size` minutes apart), each at the point it fell in, oldest first. */
export function deployMarks(deploys: Deployment[], minutes: number[], size: number): Mark[] {
  if (!minutes.length) return [];
  const first = minutes[0];
  const end = minutes[minutes.length - 1] + size;
  const marks: Mark[] = [];
  for (const d of deploys) {
    // Live once: what's live now, or was until another replaced it.
    if ((d.status !== "active" && d.status !== "inactive") || d.finished_at == null) continue;
    const t = d.finished_at / 60_000;
    if (t < first || t >= end) continue;
    const short = d.revision ? d.revision.slice(0, 7) : d.id;
    const what = [short, d.rollback_of ? "rollback" : "", d.message.split("\n")[0] ?? ""].filter(Boolean).join(" · ");
    marks.push({ i: Math.min(minutes.length - 1, Math.floor((t - first) / size)), at: d.finished_at, short, label: d.actor ? `${what} · ${d.actor}` : what });
  }
  return marks.sort((a, b) => a.at - b.at);
}

function Chart({
  title,
  minutes,
  series,
  format,
  summary,
  agg,
  range,
  limit,
  marks,
}: {
  title: string;
  minutes: number[];
  series: Series[];
  format: (v: number) => string;
  summary: string;
  agg: Agg;
  range: Range;
  /** A ceiling the values run against (the memory limit): a dashed line, inside the axis. */
  limit?: number;
  marks: Mark[];
}) {
  const [hover, setHover] = useState<number | null>(null);
  const deployed = hover === null ? 0 : marks.filter((k) => k.i === hover).length;
  const n = minutes.length;
  const top = niceMax(Math.max(0, limit ?? 0, ...series.flatMap((s) => s.values)));
  const x = (i: number) => (n <= 1 ? W : (i / (n - 1)) * W);
  // The top 14px hold the axis label above the top gridline.
  const y = (v: number) => H - 2 - (v / top) * (H - 16);
  const path = (vs: number[]) => vs.map((v, i) => `${i ? "L" : "M"}${x(i).toFixed(1)},${y(v).toFixed(1)}`).join("");
  const at = hover ?? n - 1;

  const pick = (clientX: number, rect: DOMRect) => {
    if (!n) return;
    const i = Math.round(((clientX - rect.left) / rect.width) * (n - 1));
    setHover(Math.max(0, Math.min(n - 1, i)));
  };

  return (
    <figure className={`${panel} p-4`}>
      <figcaption className="flex items-baseline justify-between gap-2">
        <span className="text-[12px] text-muted">{title}</span>
        <span className="font-mono text-[11px] text-subtle tabular-nums">{hover !== null && minutes[hover]
            ? `${clock(minutes[hover] * 60_000, range.minutes > 60 * 24).slice(0, range.minutes > 60 * 24 ? 12 : 5)}${deployed ? ` · ${deployed > 1 ? `${deployed} deploys` : "deploy"}` : ""}`
            : `${agg} · last ${range.label}`}</span>
      </figcaption>
      <div className="mt-1 flex flex-wrap items-baseline gap-x-3">
        {series.map((s) => (
          <span key={s.label} className="inline-flex items-baseline gap-1.5">
            {series.length > 1 && <i className="inline-block size-2 rounded-[3px]" style={{ background: s.color }} aria-hidden />}
            <span className="font-mono text-[17px] font-semibold tracking-[-0.03em] tabular-nums">
              {!n ? "—" : hover !== null ? format(s.values[at] ?? 0) : format(agg === "total" ? s.values.reduce((a, v) => a + v, 0) : Math.max(0, ...s.values))}
            </span>
            {series.length > 1 && <span className="text-[11px] text-subtle">{s.label}</span>}
          </span>
        ))}
        {limit != null && <span className="text-[11px] text-subtle">of {format(limit)}</span>}
      </div>
      <div
        className="relative mt-2 text-fg outline-none"
        tabIndex={0}
        role="img"
        aria-label={`${title}: ${summary}${marks.length ? `; ${marks.length} deploy${marks.length > 1 ? "s" : ""} went live` : ""}`}
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
          {marks.map((k) => (
            <line key={k.at} x1={x(k.i)} x2={x(k.i)} y1={y(top)} y2={H} stroke="var(--muted)" strokeOpacity={0.6} strokeDasharray="2 3" vectorEffect="non-scaling-stroke" />
          ))}
          {series.map((s) => (
            <path key={s.label} d={path(s.values)} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
          ))}
          {limit != null && <line x1={0} x2={W} y1={y(limit)} y2={y(limit)} stroke="var(--muted)" strokeDasharray="3 3" vectorEffect="non-scaling-stroke" />}
          {hover !== null && <line x1={x(hover)} x2={x(hover)} y1={0} y2={H} stroke="var(--line-3)" vectorEffect="non-scaling-stroke" />}
        </svg>
        <span className="pointer-events-none absolute top-0 left-0 -translate-y-0.5 font-mono text-[10px] leading-none text-subtle">{format(top)}</span>
      </div>
    </figure>
  );
}

type RouteSort = "requests" | "errors" | "p95_ms";
const ROUTE_SORTS: { key: RouteSort; label: string }[] = [
  { key: "requests", label: "busiest" },
  { key: "errors", label: "most 5xx" },
  { key: "p95_ms", label: "slowest" },
];

/** Errors and latency by route (Vercel's Observability): the last hour's access log, ids folded. */
function Routes({ stats }: { stats: RouteStats }) {
  const [sort, setSort] = useState<RouteSort>("requests");
  const rows = useMemo(() => {
    const keep = (r: RouteStat) => (sort === "errors" ? r.errors > 0 : sort === "p95_ms" ? r.requests >= 5 : true);
    return stats.routes.filter(keep).sort((a, b) => b[sort] - a[sort] || b.requests - a.requests).slice(0, 15);
  }, [stats, sort]);
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Routes">
      <div className="flex flex-wrap items-center justify-between gap-2 border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Routes, the last hour <span className="text-subtle">· {count(stats.requests)} requests{stats.cut ? ", the newest read" : ""}; ids folded as :id</span>
        </p>
        <div className="flex rounded-lg border border-line p-0.5" role="radiogroup" aria-label="Sort routes">
          {ROUTE_SORTS.map((s) => (
            <button
              key={s.key}
              role="radio"
              aria-checked={sort === s.key}
              onClick={() => setSort(s.key)}
              className={`h-6 rounded-md px-2 text-[11.5px] ${sort === s.key ? "bg-fg/[0.08] text-fg" : "text-muted hover:text-fg"}`}
            >
              {s.label}
            </button>
          ))}
        </div>
      </div>
      {rows.length === 0 ? (
        <p className="px-4 py-3 text-[12.5px] text-subtle">{stats.requests ? (sort === "errors" ? "No route answered a 5xx." : "No route had 5 requests yet.") : "No requests in the last hour."}</p>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full font-mono text-[11.5px] tabular-nums">
            <thead className="text-left text-subtle">
              <tr>
                {["route", "req", "5xx", "4xx", "p50", "p95"].map((h, i) => (
                  <th key={h} className={`px-4 py-1.5 font-normal ${i ? "text-right" : ""}`}>
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={`${r.method} ${r.route}`} className="border-t border-line">
                  <td className="max-w-[28rem] truncate px-4 py-1" title={`${r.method} ${r.route}`}>
                    <span className="text-subtle">{r.method}</span> {r.route}
                  </td>
                  <td className="px-4 py-1 text-right">{count(r.requests)}</td>
                  <td className={`px-4 py-1 text-right ${r.errors ? "text-danger" : "text-subtle"}`}>{r.errors ? count(r.errors) : "—"}</td>
                  <td className="px-4 py-1 text-right text-muted">{r.client_errors ? count(r.client_errors) : "—"}</td>
                  <td className="px-4 py-1 text-right text-muted">{formatMs(r.p50_ms)}</td>
                  <td className="px-4 py-1 text-right">{formatMs(r.p95_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

/** Requests, errors, latency, CPU, memory, instances and cold starts over the last hour, day or week. */
export function MetricsTab({ service, env, memoryMb }: { service: string; env: string; /** Its memory limit, MB. */ memoryMb?: number }) {
  const [range, setRange] = useState<Range>(RANGES[0]);
  const [raw, setM] = useState<MetricsData | null>(null);
  const [deploys, setDeploys] = useState<Deployment[]>([]);
  const [routes, setRoutes] = useState<RouteStats | null>(null);
  const size = raw ? Math.max(1, Math.ceil(raw.minutes.length / MAX_POINTS)) : 1;
  const m = useMemo(() => (raw ? bucketed(raw, size) : null), [raw, size]);
  const per = size === 1 ? "min" : `${size} min`;
  const [error, setError] = useState<string | null>(null);
  const [table, setTable] = useState(false);
  const { copied, copy } = useCopy();
  const scrape = appsApi.prometheusUrl(service, env);

  const visible = usePageVisible();
  // Another environment's deploys and routes never stay on this one's charts.
  useEffect(() => {
    setDeploys([]);
    setRoutes(null);
  }, [service, env]);
  useEffect(() => {
    if (!visible) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        // The deploys only mark the charts: metrics still show without them.
        const [next, deps, byRoute] = await Promise.all([appsApi.metrics(service, env, range.minutes), appsApi.deploys(service, env).catch(() => null), appsApi.routes(service, env).catch(() => null)]);
        if (alive) {
          setM(next);
          if (deps) setDeploys(deps);
          if (byRoute) setRoutes(byRoute);
          setError(null);
        }
      } catch (e) {
        if (alive) setError(message(e));
      }
      // A day or a week changes slowly: looked at less often.
      if (alive) timer = setTimeout(load, range.minutes > 60 ? 120_000 : 30_000);
    };
    void load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [service, env, visible, range]);

  const marks = useMemo(() => (m ? deployMarks(deploys, m.minutes, size) : []), [deploys, m, size]);
  const ink = "var(--soft)";
  const charts = useMemo(() => {
    if (!m) return [];
    const sum = (a: number[]) => a.reduce((s, v) => s + v, 0);
    const peak = (a: number[]) => Math.max(0, ...a);
    return [
      { title: `Requests / ${per}`, agg: "total" as Agg, series: [{ label: "requests", values: m.requests, color: ink }], format: count, summary: `${sum(m.requests)} in the ${range.label}` },
      { title: `5xx / ${per}`, agg: "total" as Agg, series: [{ label: "5xx", values: m.errors, color: ink }], format: count, summary: `${sum(m.errors)} in the ${range.label}` },
      {
        title: "Latency",
        agg: "peak" as Agg,
        series: [
          { label: "p50", values: m.p50_ms, color: "var(--viz-2)" },
          { label: "p95", values: m.p95_ms, color: "var(--viz-3)" },
          { label: "p99", values: m.p99_ms, color: "var(--viz-4)" },
        ],
        format: formatMs,
        summary: `p99 peaked at ${formatMs(peak(m.p99_ms))}`,
      },
      { title: "CPU (cores)", agg: "peak" as Agg, series: [{ label: "cpu", values: m.cpu, color: ink }], format: cores, summary: `peak ${cores(peak(m.cpu))} cores` },
      { title: "Memory", agg: "peak" as Agg, series: [{ label: "memory", values: m.memory_bytes, color: ink }], format: (v: number) => formatBytes(v), summary: `peak ${formatBytes(peak(m.memory_bytes))}${memoryMb ? ` of ${formatBytes(memoryMb * 1024 * 1024)}` : ""}`, limit: memoryMb ? memoryMb * 1024 * 1024 : undefined },
      { title: `Egress / ${per}`, agg: "total" as Agg, series: [{ label: "egress", values: m.egress_bytes ?? m.minutes.map(() => 0), color: ink }], format: (v: number) => formatBytes(v), summary: `${formatBytes(sum(m.egress_bytes ?? []))} answered in the ${range.label}` },
      { title: "Instances", agg: "peak" as Agg, series: [{ label: "instances", values: m.instances, color: ink }], format: count, summary: `peak ${peak(m.instances)}` },
      { title: `Cold starts / ${per}`, agg: "total" as Agg, series: [{ label: "cold starts", values: m.cold_starts, color: ink }], format: count, summary: `${sum(m.cold_starts)} in the ${range.label}` },
      { title: "Cold start time", agg: "peak" as Agg, series: [{ label: "cold start", values: m.cold_start_ms, color: ink }], format: formatMs, summary: `slowest ${formatMs(peak(m.cold_start_ms))}` },
    ];
  }, [m, range, per, memoryMb]);

  if (error && !m) return <p className="text-[13px] text-danger">Metrics didn't load: {error}</p>;
  if (!m) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading metrics…</span></p>;

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-[12px] text-subtle">
          {size === 1 ? "Per minute" : `Per ${size} minutes`}, the last {range.label}. Asleep minutes are zeros: nothing ran.
        </p>
        <div className="flex items-center gap-2">
          <div className="flex rounded-lg border border-line p-0.5" role="radiogroup" aria-label="Range">
            {RANGES.map((r) => (
              <button
                key={r.short}
                role="radio"
                aria-checked={range.short === r.short}
                onClick={() => setRange(r)}
                className={`h-6 rounded-md px-2 font-mono text-[11.5px] ${range.short === r.short ? "bg-fg/[0.08] text-fg" : "text-muted hover:text-fg"}`}
              >
                {r.short}
              </button>
            ))}
          </div>
        <button className={btn} onClick={() => setTable((t) => !t)} aria-pressed={table}>
          <Table2 className="size-3.5" /> {table ? "Charts" : "Table"}
        </button>
        </div>
      </div>
      {table ? (
        <div className={`${panel} max-h-[560px] overflow-auto`}>
          <table className="w-full font-mono text-[11.5px] tabular-nums">
            <thead className="sticky top-0 bg-panel text-left text-subtle">
              <tr>
                {["minute", "req", "5xx", "p50", "p95", "p99", "cpu", "memory", "egress", "inst", "cold", "cold ms", "deploy"].map((h) => (
                  <th key={h} className="border-b border-line px-3 py-2 font-normal">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {m.minutes.map((t, i) => (
                <tr key={t} className="border-b border-line last:border-0">
                  <td className="px-3 py-1 text-muted">{clock(t * 60_000, range.minutes > 60 * 24)}</td>
                  <td className="px-3 py-1">{m.requests[i]}</td>
                  <td className="px-3 py-1">{m.errors[i]}</td>
                  <td className="px-3 py-1">{formatMs(m.p50_ms[i])}</td>
                  <td className="px-3 py-1">{formatMs(m.p95_ms[i])}</td>
                  <td className="px-3 py-1">{formatMs(m.p99_ms[i])}</td>
                  <td className="px-3 py-1">{cores(m.cpu[i] ?? 0)}</td>
                  <td className="px-3 py-1">{formatBytes(m.memory_bytes[i])}</td>
                  <td className="px-3 py-1">{formatBytes(m.egress_bytes?.[i] ?? 0)}</td>
                  <td className="px-3 py-1">{m.instances[i]}</td>
                  <td className="px-3 py-1">{m.cold_starts[i]}</td>
                  <td className="px-3 py-1">{m.cold_starts[i] ? formatMs(m.cold_start_ms[i]) : "—"}</td>
                  <td className="px-3 py-1 text-muted">{marks.filter((k) => k.i === i).map((k) => k.short).join(", ")}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          {charts.map((c) => (
            <Chart key={c.title} minutes={m.minutes} range={range} marks={marks} {...c} />
          ))}
        </div>
      )}
      {marks.length > 0 && (
        <div className="text-[12px] text-subtle">
          <span>Dashed: a deploy went live.</span>
          <ul className="mt-1 space-y-0.5">
            {marks.slice(-6).map((k) => (
              <li key={k.at} className="flex min-w-0 gap-2">
                <span className="shrink-0 font-mono tabular-nums">{clock(k.at, range.minutes > 60 * 24).slice(0, range.minutes > 60 * 24 ? 12 : 5)}</span>
                <span className="min-w-0 truncate text-muted" title={k.label}>
                  {k.label}
                </span>
              </li>
            ))}
          </ul>
          {marks.length > 6 && <p className="mt-0.5">and {marks.length - 6} earlier (the table marks them)</p>}
        </div>
      )}
      {routes && <Routes stats={routes} />}
      <p className="flex flex-wrap items-center gap-x-2 gap-y-1 text-[12px] text-subtle">
        <span>For a scraper:</span>
        <code className="min-w-0 truncate font-mono text-[11.5px] text-muted">{scrape}</code>
        <CopyButton id="scrape" text={scrape} copied={copied} onCopy={copy} />
        <span>
          the last finished minute as Prometheus text, read with a token that may read the service (<code className="font-mono">roll token create</code>).
        </span>
      </p>
    </div>
  );
}
