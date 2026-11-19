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
import { appsApi, formatMs, type Analytics, type Deployment, type Metrics as MetricsData, type RouteStat, type RouteStats, type Span, type TraceSummary, type Vital, type VitalName, type Vitals, type AppMetrics, type AppSeries, type ErrorGroup, type SplitStat, type InstanceSeries, type SloIndicator, type SloReport } from "@/lib/apps-api";
import { formatBytes } from "@/lib/roll-api";
import { btn, CopyButton, panel, useCopy } from "../ui";
import { usePageVisible } from "./live";
import { clock, message, since } from "./shared";

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
  // Past a week rolld answers a point an hour (its hourly rollups, kept 90 days).
  { minutes: 30 * 24 * 60, label: "30 days", short: "30d" },
  { minutes: 90 * 24 * 60, label: "90 days", short: "90d" },
] as const;
type Range = (typeof RANGES)[number];

/** At most this many points a chart: a week of minutes in 28-minute buckets. */
const MAX_POINTS = 360;

const COUNTS = ["requests", "errors", "cold_starts", "egress_bytes", "over_capacity"] as const;
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
/** Recent traces (OpenTelemetry, sent by the app's instances): one opens as a waterfall. */
function Traces({ service, env, traces }: { service: string; env: string; traces: TraceSummary[] }) {
  const [open, setOpen] = useState<string | null>(null);
  const [spans, setSpans] = useState<Span[] | null>(null);
  useEffect(() => {
    if (!open) return;
    let alive = true;
    setSpans(null);
    appsApi.trace(service, env, open).then(
      (t) => alive && setSpans(t.spans),
      () => alive && setSpans([]),
    );
    return () => {
      alive = false;
    };
  }, [service, env, open]);
  const start = spans && spans.length ? Math.min(...spans.map((s) => s.start)) : 0;
  const total = spans && spans.length ? Math.max(1, Math.max(...spans.map((s) => s.end)) - start) : 1;
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Traces">
      <div className="border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Traces <span className="text-subtle">· the {traces.length} newest, from the app's OpenTelemetry SDK</span>
        </p>
      </div>
      <ul className="divide-y divide-line">
        {traces.slice(0, 20).map((t) => (
          <li key={t.trace}>
            <button type="button" className="flex w-full items-center justify-between gap-3 px-4 py-2 text-left text-[12px] hover:bg-panel-2" aria-expanded={open === t.trace} onClick={() => setOpen(open === t.trace ? null : t.trace)}>
              <span className="min-w-0 truncate">
                <span className={t.error ? "text-danger" : ""}>{t.name}</span> <span className="text-subtle">· {t.spans} spans</span>
              </span>
              <span className="font-mono text-muted">{formatMs(t.ms)}</span>
            </button>
            {open === t.trace && (
              <div className="space-y-1 px-4 pb-3">
                {spans === null && <p className="shimmer text-[12px] text-muted">Reading its spans…</p>}
                {spans?.map((s) => (
                  <div key={s.span} className="grid grid-cols-[minmax(0,14rem)_1fr_4rem] items-center gap-2 text-[11.5px]">
                    <span className={`truncate font-mono ${s.error ? "text-danger" : "text-muted"}`} title={s.name}>
                      {s.name}
                    </span>
                    <span className="relative h-2 rounded-sm bg-line">
                      <span
                        className={`absolute inset-y-0 rounded-sm ${s.error ? "bg-danger" : "bg-accent"}`}
                        style={{ left: `${((s.start - start) / total) * 100}%`, width: `${Math.max(0.5, ((s.end - s.start) / total) * 100)}%` }}
                      />
                    </span>
                    <span className="text-right font-mono text-subtle">{formatMs((s.end - s.start) / 1e6)}</span>
                  </div>
                ))}
              </div>
            )}
          </li>
        ))}
      </ul>
    </section>
  );
}

/** Visitors and page views from the access log: no script, no cookie (an address and browser a day). */
function Visitors({ a }: { a: Analytics }) {
  const lists: { title: string; rows: { label: string; n: number }[] }[] = [
    { title: "Pages", rows: a.pages.map((p) => ({ label: p.path, n: p.views })) },
    { title: "Referrers", rows: a.referrers.map((r) => ({ label: r.host, n: r.views })) },
    { title: "Countries", rows: a.countries.map((c) => ({ label: c.country, n: c.views })) },
    { title: "Campaigns", rows: (a.campaigns ?? []).map((c) => ({ label: [c.source, c.medium, c.campaign].filter(Boolean).join(" / "), n: c.views })) },
    { title: "Devices", rows: (a.devices?.classes ?? []).map((d) => ({ label: d.class, n: d.views })) },
    { title: "Browsers", rows: (a.devices?.browsers ?? []).map((b) => ({ label: b.browser, n: b.views })) },
    {
      title: "Events",
      rows: (a.events ?? []).map((e) => {
        const [k, values] = Object.entries(e.props)[0] ?? [];
        return { label: k && values?.[0] ? `${e.name} · ${k}: ${values[0].value}` : e.name, n: e.count };
      }),
    },
  ];
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Visitors">
      <div className="border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Visitors <span className="text-subtle">· {count(a.visitors)} visitors, {count(a.page_views)} page views{a.cut ? ", the newest read" : ""}; from the access log, no script or cookie (events: <code className="font-mono">roll.track()</code> from /__roll/track.js)</span>
        </p>
      </div>
      <div className="grid gap-px bg-line sm:grid-cols-3">
        {/* Pages, referrers and countries always; the rest once there's something to show. */}
        {lists.filter((l, i) => i < 3 || l.rows.length > 0).map((l) => (
          <div key={l.title} className="bg-panel px-4 py-3">
            <p className="mb-2 text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">{l.title}</p>
            {l.rows.length === 0 ? (
              <p className="text-[12px] text-subtle">None yet.</p>
            ) : (
              <ul className="space-y-1">
                {l.rows.slice(0, 8).map((r) => (
                  <li key={r.label} className="flex items-center justify-between gap-3 text-[12px]">
                    <span className="min-w-0 truncate font-mono text-[11.5px]">{r.label}</span>
                    <span className="font-mono text-muted">{count(r.n)}</span>
                  </li>
                ))}
              </ul>
            )}
          </div>
        ))}
      </div>
    </section>
  );
}

function Sparkline({ points }: { points: [number, number][] }) {
  if (points.length < 2) return <span className="text-[11px] text-subtle">–</span>;
  const xs = points.map((p) => p[0]);
  const ys = points.map((p) => p[1]);
  const [x0, x1] = [Math.min(...xs), Math.max(...xs)];
  const [y0, y1] = [Math.min(...ys), Math.max(...ys)];
  const d = points.map((p, i) => `${i ? "L" : "M"}${(((p[0] - x0) / (x1 - x0 || 1)) * 120).toFixed(1)},${(22 - ((p[1] - y0) / (y1 - y0 || 1)) * 20).toFixed(1)}`).join("");
  return (
    <svg viewBox="0 0 120 24" className="h-6 w-[120px] text-muted" aria-hidden>
      <path d={d} fill="none" stroke="currentColor" strokeWidth="1.5" />
    </svg>
  );
}

function shown(s: AppSeries) {
  const v = s.points[s.points.length - 1]?.[1];
  if (v === undefined) return "–";
  const n = Math.abs(v) >= 100 ? v.toFixed(0) : Math.abs(v) >= 1 ? v.toFixed(2) : v.toPrecision(2);
  return `${n}${s.unit && s.unit !== "1" ? ` ${s.unit}` : ""}${s.shows === "per_second" ? "/s" : ""}`;
}

/** Its errors, grouped from its log (Cloud Run's Error Reporting): the newest of each opens whole. */
/** Its objective against its window (Cloud Monitoring's SLO view): each indicator's share and the budget left. */
function Budget({ r }: { r: SloReport }) {
  if (!r.slo) return null;
  const row = (label: string, i: SloIndicator | null | undefined) => {
    if (!i) return null;
    const left = i.budget_left;
    const pct = left == null ? null : Math.round(left * 1000) / 10;
    return (
      <li className="flex flex-wrap items-center gap-x-3 gap-y-1 px-4 py-2.5 text-[12px]">
        <span className="w-24 text-muted">{label}</span>
        <span className="w-20 font-mono tabular-nums">{i.sli == null ? "–" : `${i.sli.toFixed(3)}%`}</span>
        <span className="h-1.5 min-w-24 flex-1 overflow-hidden rounded-full bg-fg/[0.08]" aria-hidden>
          <span className={`block h-full rounded-full ${pct != null && pct < 0 ? "bg-danger" : "bg-accent"}`} style={{ width: `${Math.max(0, Math.min(100, pct ?? 0))}%` }} />
        </span>
        <span className={`w-36 text-right font-mono tabular-nums ${pct != null && pct < 0 ? "text-danger" : "text-subtle"}`}>{pct == null ? "no requests yet" : pct < 0 ? `budget spent (${pct}%)` : `${pct}% of budget left`}</span>
      </li>
    );
  };
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Error budget">
      <div className="flex flex-wrap items-center gap-x-3 border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Error budget <span className="text-subtle">· {r.slo.objective}% over {r.slo.window_days ?? 28} days{r.slo.latency_ms ? `, p95 under ${r.slo.latency_ms} ms` : ""}</span>
        </p>
        {r.burning && (
          <span className="ml-auto text-[12px] text-danger">
            burning {r.burning.burn_rate}× over the last {r.burning.window === "1h" ? "hour" : "six hours"} ({r.burning.indicator})
          </span>
        )}
      </div>
      <ul className="divide-y divide-line">
        {row("Availability", r.availability)}
        {row("Latency", r.latency)}
      </ul>
    </section>
  );
}

function Errors({ groups }: { groups: ErrorGroup[] }) {
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Errors">
      <div className="border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Errors <span className="text-subtle">· from its log, stack traces joined and grouped</span>
        </p>
      </div>
      <ul className="divide-y divide-line">
        {groups.slice(0, 15).map((g) => (
          <li key={g.fingerprint} className="px-4 py-2">
            <details>
              <summary className="flex cursor-pointer flex-wrap items-center gap-x-3 gap-y-1 text-[12px] [&::-webkit-details-marker]:hidden">
                <span className="w-12 text-right font-mono text-danger">{count(g.count)}×</span>
                <span className="min-w-0 flex-1 truncate font-mono text-[11.5px]">{g.title}</span>
                <span className="text-[11px] text-subtle">
                  {g.kind === "line" ? "line" : `${g.kind} trace`} · {since(g.last_seen)}
                  {g.instances > 1 ? ` · ${g.instances} instances` : ""}
                </span>
              </summary>
              <pre className="mt-2 max-h-72 overflow-auto rounded-md border border-line bg-bg p-3 font-mono text-[11px] leading-relaxed text-muted">{g.sample}</pre>
            </details>
          </li>
        ))}
      </ul>
    </section>
  );
}

/** The app's own metrics, from its OpenTelemetry SDK (traces on). */
function AppMetricsPanel({ m }: { m: AppMetrics }) {
  return (
    <section className={`${panel} overflow-hidden`} aria-label="App metrics">
      <div className="border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          App metrics <span className="text-subtle">· from its OpenTelemetry SDK, {m.step_minutes}-minute points{m.cut ? ", the newest read" : ""}</span>
        </p>
      </div>
      <ul className="divide-y divide-line">
        {m.series.slice(0, 20).map((s) => (
          <li key={s.name + s.attrs.map((a) => `|${a.key}=${a.value}`).join("")} className="flex flex-wrap items-center gap-x-4 gap-y-1 px-4 py-2 text-[12px]">
            <span className="min-w-0 flex-1 truncate font-mono text-[11.5px]">
              {s.name}
              {s.attrs.length > 0 && <span className="text-subtle"> {s.attrs.map((a) => `${a.key}=${a.value}`).join(" ")}</span>}
            </span>
            <span className="text-[11px] text-subtle">{s.shows === "per_second" ? "rate" : s.shows === "mean" ? "mean" : s.kind}</span>
            <Sparkline points={s.points} />
            <span className="w-24 text-right font-mono">{shown(s)}</span>
          </li>
        ))}
      </ul>
    </section>
  );
}

const VITALS: { key: VitalName; label: string; unit: "ms" | "" }[] = [
  { key: "lcp", label: "Largest paint", unit: "ms" },
  { key: "inp", label: "Interaction", unit: "ms" },
  { key: "cls", label: "Layout shift", unit: "" },
  { key: "fcp", label: "First paint", unit: "ms" },
  { key: "ttfb", label: "First byte", unit: "ms" },
];

const RATED: Record<Vital["rating"], string> = { good: "text-fg", "needs-improvement": "text-muted", poor: "text-danger" };

function vital(v: Vital | undefined, unit: "ms" | "") {
  if (!v) return "–";
  return unit === "ms" ? (v.p75 >= 1000 ? `${(v.p75 / 1000).toFixed(2)} s` : `${Math.round(v.p75)} ms`) : v.p75.toFixed(3);
}

/** What visitors' browsers measured (Web Vitals on): each metric's p75, and the busiest routes'. */
function Speed({ v }: { v: Vitals }) {
  return (
    <section className={`${panel} overflow-hidden`} aria-label="Speed">
      <div className="border-b border-line px-4 py-2.5">
        <p className="text-[12px] text-muted">
          Speed <span className="text-subtle">· p75 of {count(v.samples)} page loads in visitors' browsers{v.cut ? ", the newest read" : ""}</span>
        </p>
      </div>
      <div className="grid grid-cols-2 gap-px bg-line sm:grid-cols-5">
        {VITALS.map((m) => {
          const x = v.metrics[m.key];
          return (
            <div key={m.key} className="bg-panel px-4 py-3">
              <p className="text-[11px] font-medium uppercase tracking-[0.06em] text-subtle">{m.label}</p>
              <p className={`mt-1 font-mono text-[18px] tracking-[-0.04em] ${x ? RATED[x.rating] : "text-subtle"}`}>{vital(x, m.unit)}</p>
              <p className="text-[11px] text-subtle">{x ? x.rating.replace("-", " ") : "not measured yet"}</p>
            </div>
          );
        })}
      </div>
      {v.routes.length > 0 && (
        <div className="overflow-x-auto">
          <table className="w-full text-[12px]">
            <thead>
              <tr className="text-left text-[11px] text-subtle">
                <th className="px-4 py-2 font-medium">Route</th>
                {VITALS.map((m) => (
                  <th key={m.key} className="px-2 py-2 text-right font-medium uppercase">
                    {m.key}
                  </th>
                ))}
                <th className="px-4 py-2 text-right font-medium">Loads</th>
              </tr>
            </thead>
            <tbody>
              {v.routes.slice(0, 10).map((r) => (
                <tr key={r.route} className="border-t border-line">
                  <td className="max-w-[16rem] truncate px-4 py-1.5 font-mono text-[11.5px]">{r.route}</td>
                  {VITALS.map((m) => {
                    const x = r.metrics[m.key];
                    return (
                      <td key={m.key} className={`px-2 py-1.5 text-right font-mono ${x ? RATED[x.rating] : "text-subtle"}`}>
                        {vital(x, m.unit)}
                      </td>
                    );
                  })}
                  <td className="px-4 py-1.5 text-right font-mono text-muted">{count(r.samples)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </section>
  );
}

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

/** Each instance's own CPU and memory over the last hour (Railway's per replica): now, the
 *  hour's peak, and its line. Against its limits when they're known. */
function Instances({ rows, cpus, memoryMb }: { rows: InstanceSeries[]; cpus?: number; memoryMb?: number }) {
  const last = <T,>(v: T[]) => v[v.length - 1];
  return (
    <section className={`${panel} overflow-hidden`} aria-label="By instance">
      <p className="border-b border-line px-4 py-2.5 text-[12px] text-muted">CPU and memory by instance, the last hour</p>
      <table className="w-full font-mono text-[11.5px] tabular-nums">
        <thead className="text-left text-subtle">
          <tr>
            {["", "CPU", "peak", "", "memory", "peak", ""].map((h, i) => (
              <th key={i} className={`px-4 py-1.5 font-normal ${i ? "text-right" : ""}`}>
                {h}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {rows.slice(0, 20).map((r) => {
            const pct = (c: number) => (cpus ? `${Math.round((c / cpus) * 100)}%` : cores(c));
            const mem = (b: number) => (memoryMb ? `${formatBytes(b)} (${Math.round((b / (memoryMb * 1048576)) * 100)}%)` : formatBytes(b));
            return (
              <tr key={r.instance} className="border-t border-line">
                <td className="truncate px-4 py-1" title={`${r.instance} (${r.deployment})`}>
                  {r.instance.split("-").slice(-2).join("-")}
                </td>
                <td className="px-4 py-1 text-right">{pct(last(r.cpu) ?? 0)}</td>
                <td className="px-4 py-1 text-right text-muted">{pct(Math.max(...r.cpu))}</td>
                <td className="px-2 py-1">
                  <Sparkline points={r.minutes.map((m, i) => [m, r.cpu[i]] as [number, number])} />
                </td>
                <td className="px-4 py-1 text-right">{mem(last(r.memory_bytes) ?? 0)}</td>
                <td className="px-4 py-1 text-right text-muted">{formatBytes(Math.max(...r.memory_bytes))}</td>
                <td className="px-2 py-1">
                  <Sparkline points={r.minutes.map((m, i) => [m, r.memory_bytes[i]] as [number, number])} />
                </td>
              </tr>
            );
          })}
        </tbody>
      </table>
    </section>
  );
}

/** The last hour's requests by the deployment, and the instance, that answered (shown when more than one did). */
function Splits({ stats }: { stats: RouteStats }) {
  const parts = [
    { title: "By deployment", rows: stats.deployments ?? [], name: (s: SplitStat) => (s.deployment ?? "").replace(/^dep_/, "").slice(0, 8) },
    { title: "By instance", rows: stats.instances ?? [], name: (s: SplitStat) => (s.instance ?? "").split("-").slice(-2).join("-") },
  ].filter((p) => p.rows.length > 1);
  if (!parts.length) return null;
  return (
    <section className={`${panel} grid gap-0 overflow-hidden md:grid-cols-2`} aria-label="By deployment and instance">
      {parts.map((p) => (
        <div key={p.title} className="min-w-0 border-line md:[&:not(:first-child)]:border-l">
          <p className="border-b border-line px-4 py-2.5 text-[12px] text-muted">{p.title}, the last hour</p>
          <table className="w-full font-mono text-[11.5px] tabular-nums">
            <thead className="text-left text-subtle">
              <tr>
                {["", "req", "5xx", "p50", "p95"].map((h, i) => (
                  <th key={h || "name"} className={`px-4 py-1.5 font-normal ${i ? "text-right" : ""}`}>
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {p.rows.slice(0, 8).map((r) => (
                <tr key={r.deployment ?? r.instance} className="border-t border-line">
                  <td className="truncate px-4 py-1" title={r.deployment ?? r.instance}>
                    {p.name(r)}
                  </td>
                  <td className="px-4 py-1 text-right">{count(r.requests)}</td>
                  <td className={`px-4 py-1 text-right ${r.errors ? "text-danger" : "text-subtle"}`}>{r.errors ? count(r.errors) : "—"}</td>
                  <td className="px-4 py-1 text-right text-muted">{formatMs(r.p50_ms)}</td>
                  <td className="px-4 py-1 text-right">{formatMs(r.p95_ms)}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ))}
    </section>
  );
}

/** Requests, errors, latency, CPU, memory, instances and cold starts over the last hour, day or week. */
export function MetricsTab({
  service,
  env,
  memoryMb,
  maxInstances,
  cpus,
}: {
  service: string;
  env: string;
  /** Its memory limit, MB. */
  memoryMb?: number;
  /** The most instances it scales to. */
  maxInstances?: number;
  /** The cores each instance gets. */
  cpus?: number;
}) {
  const [range, setRange] = useState<Range>(RANGES[0]);
  const [raw, setM] = useState<MetricsData | null>(null);
  const [deploys, setDeploys] = useState<Deployment[]>([]);
  const [routes, setRoutes] = useState<RouteStats | null>(null);
  const [visitors, setVisitors] = useState<Analytics | null>(null);
  const [speed, setSpeed] = useState<Vitals | null>(null);
  const [appMetrics, setAppMetrics] = useState<AppMetrics | null>(null);
  const [errorGroups, setErrorGroups] = useState<ErrorGroup[]>([]);
  const [budget, setBudget] = useState<SloReport | null>(null);
  const [traces, setTraces] = useState<TraceSummary[] | null>(null);
  const size = raw ? Math.max(1, Math.ceil(raw.minutes.length / MAX_POINTS)) : 1;
  const m = useMemo(() => (raw ? bucketed(raw, size) : null), [raw, size]);
  // Minutes a point stands for: rolld's step (an hour past a week) times the buckets here.
  const span = size * (raw?.step ?? 1);
  const per = span === 1 ? "min" : span === 60 ? "hour" : span % 60 === 0 ? `${span / 60} h` : `${span} min`;
  const [error, setError] = useState<string | null>(null);
  const [table, setTable] = useState(false);
  const { copied, copy } = useCopy();
  const scrape = appsApi.prometheusUrl(service, env);

  const visible = usePageVisible();
  // Another environment's deploys and routes never stay on this one's charts.
  useEffect(() => {
    setDeploys([]);
    setRoutes(null);
    setBudget(null);
  }, [service, env]);
  useEffect(() => {
    if (!visible) return;
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        // The deploys only mark the charts: metrics still show without them.
        const [next, deps, byRoute, seen, traced, measured, own, errs, slo] = await Promise.all([
          appsApi.metrics(service, env, range.minutes),
          appsApi.deploys(service, env).catch(() => null),
          appsApi.routes(service, env).catch(() => null),
          appsApi.analytics(service, env, Math.min(Math.max(range.minutes, 60), 10_080)).catch(() => null),
          appsApi.traces(service, env, Math.min(Math.max(range.minutes, 5), 10_080)).catch(() => null),
          appsApi.vitals(service, env, Math.min(Math.max(range.minutes, 60), 10_080)).catch(() => null),
          appsApi.appMetrics(service, env, Math.min(Math.max(range.minutes, 5), 10_080)).catch(() => null),
          appsApi.errors(service, env, Math.min(Math.max(range.minutes, 5), 10_080)).catch(() => null),
          appsApi.slo(service, env).catch(() => null),
        ]);
        if (alive) {
          setM(next);
          if (deps) setDeploys(deps);
          if (byRoute) setRoutes(byRoute);
          if (seen) setVisitors(seen);
          if (traced) setTraces(traced.traces);
          if (measured) setSpeed(measured);
          if (own) setAppMetrics(own);
          if (errs) setErrorGroups(errs.groups);
          setBudget(slo);
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
      { title: "Instances", agg: "peak" as Agg, series: [{ label: "instances", values: m.instances, color: ink }], format: count, summary: `peak ${peak(m.instances)}${maxInstances ? ` of ${maxInstances}` : ""}`, limit: maxInstances },
      {
        title: `Over capacity / ${per}`,
        agg: "total" as Agg,
        series: [{ label: "over capacity", values: m.over_capacity ?? m.minutes.map(() => 0), color: ink }],
        format: count,
        summary: sum(m.over_capacity ?? []) ? `${sum(m.over_capacity ?? [])} requests found every instance busy: raise max instances or concurrency` : `none: there was always room under max instances`,
      },
      { title: `Cold starts / ${per}`, agg: "total" as Agg, series: [{ label: "cold starts", values: m.cold_starts, color: ink }], format: count, summary: `${sum(m.cold_starts)} in the ${range.label}` },
      { title: "Cold start time", agg: "peak" as Agg, series: [{ label: "cold start", values: m.cold_start_ms, color: ink }], format: formatMs, summary: `slowest ${formatMs(peak(m.cold_start_ms))}` },
    ];
  }, [m, range, per, memoryMb, maxInstances]);

  if (error && !m) return <p className="text-[13px] text-danger">Metrics didn't load: {error}</p>;
  if (!m) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading metrics…</span></p>;

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-[12px] text-subtle">
          {span === 1 ? "Per minute" : span === 60 ? "Per hour" : span % 60 === 0 ? `Per ${span / 60} hours` : `Per ${span} minutes`}, the last {range.label}. Asleep minutes are zeros: nothing ran.
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
      {budget?.slo && <Budget r={budget} />}
      {routes && <Routes stats={routes} />}
      {routes && <Splits stats={routes} />}
      {(raw?.by_instance?.length ?? 0) > 0 && <Instances rows={raw!.by_instance!} cpus={cpus} memoryMb={memoryMb} />}
      {errorGroups.length > 0 && <Errors groups={errorGroups} />}
      {speed && speed.samples > 0 && <Speed v={speed} />}
      {appMetrics && appMetrics.series.length > 0 && <AppMetricsPanel m={appMetrics} />}
      {visitors && visitors.page_views > 0 && <Visitors a={visitors} />}
      {traces && traces.length > 0 && <Traces service={service} env={env} traces={traces} />}
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
