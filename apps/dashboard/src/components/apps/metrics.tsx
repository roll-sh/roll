/**
 * The last hour per minute as small multiples, drawn by the dataviz skill's rules: one measure per
 * chart (one axis each, never two scales), single series in neutral ink with no legend, the
 * latency chart's three percentiles in the validated chart tokens (`--viz-1…3`) with a legend that
 * carries each latest value, hairline grid, a crosshair and a value-first tooltip on hover or
 * focus, and the same numbers as a table.
 */

import { Table2 } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { appsApi, formatMs, type Metrics as MetricsData } from "@/lib/apps-api";
import { formatBytes } from "@/lib/roll-api";
import { btn, panel } from "../ui";
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

function Chart({ title, minutes, series, format, summary, agg }: { title: string; minutes: number[]; series: Series[]; format: (v: number) => string; summary: string; agg: Agg }) {
  const [hover, setHover] = useState<number | null>(null);
  const n = minutes.length;
  const top = niceMax(Math.max(0, ...series.flatMap((s) => s.values)));
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
        <span className="font-mono text-[11px] text-subtle tabular-nums">{hover !== null && minutes[hover] ? clock(minutes[hover] * 60_000).slice(0, 5) : `${agg} · last hour`}</span>
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
      </div>
      <div
        className="relative mt-2 text-fg outline-none"
        tabIndex={0}
        role="img"
        aria-label={`${title}: ${summary}`}
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
          {series.map((s) => (
            <path key={s.label} d={path(s.values)} fill="none" stroke={s.color} strokeWidth={2} strokeLinejoin="round" strokeLinecap="round" vectorEffect="non-scaling-stroke" />
          ))}
          {hover !== null && <line x1={x(hover)} x2={x(hover)} y1={0} y2={H} stroke="var(--line-3)" vectorEffect="non-scaling-stroke" />}
        </svg>
        <span className="pointer-events-none absolute top-0 left-0 -translate-y-0.5 font-mono text-[10px] leading-none text-subtle">{format(top)}</span>
      </div>
    </figure>
  );
}

/** Requests, errors, latency, CPU, memory, instances and cold starts over the last hour. */
export function MetricsTab({ service, env }: { service: string; env: string }) {
  const [m, setM] = useState<MetricsData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [table, setTable] = useState(false);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const load = async () => {
      try {
        const next = await appsApi.metrics(service, env, 60);
        if (alive) {
          setM(next);
          setError(null);
        }
      } catch (e) {
        if (alive) setError(message(e));
      }
      if (alive) timer = setTimeout(load, 30_000);
    };
    void load();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [service, env]);

  const ink = "var(--soft)";
  const charts = useMemo(() => {
    if (!m) return [];
    const sum = (a: number[]) => a.reduce((s, v) => s + v, 0);
    const peak = (a: number[]) => Math.max(0, ...a);
    return [
      { title: "Requests / min", agg: "total" as Agg, series: [{ label: "requests", values: m.requests, color: ink }], format: count, summary: `${sum(m.requests)} in the hour` },
      { title: "5xx / min", agg: "total" as Agg, series: [{ label: "5xx", values: m.errors, color: ink }], format: count, summary: `${sum(m.errors)} in the hour` },
      {
        title: "Latency",
        agg: "peak" as Agg,
        series: [
          { label: "p50", values: m.p50_ms, color: "var(--viz-1)" },
          { label: "p95", values: m.p95_ms, color: "var(--viz-2)" },
          { label: "p99", values: m.p99_ms, color: "var(--viz-3)" },
        ],
        format: formatMs,
        summary: `p99 peaked at ${formatMs(peak(m.p99_ms))}`,
      },
      { title: "CPU (cores)", agg: "peak" as Agg, series: [{ label: "cpu", values: m.cpu, color: ink }], format: cores, summary: `peak ${cores(peak(m.cpu))} cores` },
      { title: "Memory", agg: "peak" as Agg, series: [{ label: "memory", values: m.memory_bytes, color: ink }], format: (v: number) => formatBytes(v), summary: `peak ${formatBytes(peak(m.memory_bytes))}` },
      { title: "Instances", agg: "peak" as Agg, series: [{ label: "instances", values: m.instances, color: ink }], format: count, summary: `peak ${peak(m.instances)}` },
      { title: "Cold starts / min", agg: "total" as Agg, series: [{ label: "cold starts", values: m.cold_starts, color: ink }], format: count, summary: `${sum(m.cold_starts)} in the hour` },
      { title: "Cold start time", agg: "peak" as Agg, series: [{ label: "cold start", values: m.cold_start_ms, color: ink }], format: formatMs, summary: `slowest ${formatMs(peak(m.cold_start_ms))}` },
    ];
  }, [m]);

  if (error && !m) return <p className="text-[13px] text-danger">Metrics didn't load: {error}</p>;
  if (!m) return <p className="text-[13px] text-subtle"><span className="shimmer">Loading metrics…</span></p>;

  return (
    <div className="space-y-3">
      <div className="flex items-center justify-between">
        <p className="text-[12px] text-subtle">Per minute, the last hour. Asleep minutes are zeros: nothing ran.</p>
        <button className={btn} onClick={() => setTable((t) => !t)} aria-pressed={table}>
          <Table2 className="size-3.5" /> {table ? "Charts" : "Table"}
        </button>
      </div>
      {table ? (
        <div className={`${panel} max-h-[560px] overflow-auto`}>
          <table className="w-full font-mono text-[11.5px] tabular-nums">
            <thead className="sticky top-0 bg-panel text-left text-subtle">
              <tr>
                {["minute", "req", "5xx", "p50", "p95", "p99", "cpu", "memory", "inst", "cold", "cold ms"].map((h) => (
                  <th key={h} className="border-b border-line px-3 py-2 font-normal">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {m.minutes.map((t, i) => (
                <tr key={t} className="border-b border-line last:border-0">
                  <td className="px-3 py-1 text-muted">{clock(t * 60_000).slice(0, 5)}</td>
                  <td className="px-3 py-1">{m.requests[i]}</td>
                  <td className="px-3 py-1">{m.errors[i]}</td>
                  <td className="px-3 py-1">{formatMs(m.p50_ms[i])}</td>
                  <td className="px-3 py-1">{formatMs(m.p95_ms[i])}</td>
                  <td className="px-3 py-1">{formatMs(m.p99_ms[i])}</td>
                  <td className="px-3 py-1">{cores(m.cpu[i] ?? 0)}</td>
                  <td className="px-3 py-1">{formatBytes(m.memory_bytes[i])}</td>
                  <td className="px-3 py-1">{m.instances[i]}</td>
                  <td className="px-3 py-1">{m.cold_starts[i]}</td>
                  <td className="px-3 py-1">{m.cold_starts[i] ? formatMs(m.cold_start_ms[i]) : "—"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      ) : (
        <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
          {charts.map((c) => (
            <Chart key={c.title} minutes={m.minutes} {...c} />
          ))}
        </div>
      )}
    </div>
  );
}
