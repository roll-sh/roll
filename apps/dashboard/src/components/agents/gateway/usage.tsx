"use client";

/**
 * Usage and cost (docs/code/gateway.md, "Usage and cost"), drawn by the dataviz skill's rules:
 * filters in one row above (range, group, measure), stat tiles, then stacked columns by day: the
 * four biggest groups in the chart tokens (`--viz-1…4`, validated in both modes), the rest folded
 * into one neutral "Other" (colours never cycle and follow the group, not its rank between
 * filters), 2px surface gaps between segments, a 4px rounded top, hairline grid, one axis. Hover
 * or focus a day for every group's value (value first); the legend carries each group's total and
 * the table under it every value, so nothing is colour-only or hover-only. Cost is an estimate
 * from list prices (or the owner's), not a bill.
 */

import { Table2 } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import type { Machine, UsageBy, UsageReport } from "@/lib/agents-api";
import { compact, formatMetric, niceMax, OTHER, stackUsage, usageTotals, usd, type UsageMetric } from "@/lib/agents-gateway";
import type { GatewayCommands } from "@/lib/agents-gateway";
import { cn, Segmented, Select } from "../ui";
import { useAgents, useAgentsSnapshot } from "../use-agents";
import { message, Problem } from "./shared";

const RANGES = [
  { value: "7", label: "7 days" },
  { value: "30", label: "30 days" },
  { value: "90", label: "90 days" },
] as const;
const GROUPS: { value: UsageBy; label: string }[] = [
  { value: "model", label: "By model" },
  { value: "machine", label: "By machine" },
  { value: "project", label: "By project" },
  { value: "upstream", label: "By upstream" },
];
const METRICS: { value: UsageMetric; label: string }[] = [
  { value: "cost", label: "Cost" },
  { value: "tokens", label: "Tokens" },
  { value: "requests", label: "Requests" },
];
const SLOTS = ["var(--viz-1)", "var(--viz-2)", "var(--viz-3)", "var(--viz-4)"];
const DAY = 86_400_000;

/** A group's colour: by the group's own key, kept while the filters change the ranking. */
function useColours(keys: readonly string[], scope: string) {
  const seen = useRef(new Map<string, Map<string, string>>());
  let map = seen.current.get(scope);
  if (!map) seen.current.set(scope, (map = new Map()));
  for (const k of keys) {
    if (k === OTHER || map.has(k)) continue;
    const used = new Set(keys.filter((x) => map!.has(x)).map((x) => map!.get(x)));
    const free = SLOTS.find((s) => !used.has(s));
    if (free) map.set(k, free);
  }
  return (k: string) => (k === OTHER ? "var(--viz-other)" : (map!.get(k) ?? "var(--viz-other)"));
}

export function UsageCharts({ gw, upstreamName }: { gw: GatewayCommands; upstreamName: (id: string) => string }) {
  const { client } = useAgents();
  const data = useAgentsSnapshot(client)?.data ?? client.data;
  const [range, setRange] = useState<"7" | "30" | "90">("7");
  const [by, setBy] = useState<UsageBy>("model");
  const [metric, setMetric] = useState<UsageMetric>("cost");
  const [report, setReport] = useState<UsageReport | null>(null);
  const [loading, setLoading] = useState(false);
  const [problem, setProblem] = useState<string | null>(null);
  const [table, setTable] = useState(false);

  useEffect(() => {
    let live = true;
    const to = Math.ceil(Date.now() / DAY) * DAY;
    setLoading(true);
    gw.usage(to - Number(range) * DAY, to, by, "day").then(
      (r) => {
        if (!live) return;
        setReport(r);
        setProblem(null);
        setLoading(false);
      },
      (e: unknown) => {
        if (!live) return;
        setProblem(message(e));
        setLoading(false);
      },
    );
    return () => {
      live = false;
    };
    // `gw` is rebuilt with the host's record; the query only changes with these.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [range, by]);

  const name = (key: string): string => {
    if (key === OTHER) return "Other";
    if (!key) return by === "machine" ? "Not one of your machines" : by === "project" ? "No project" : "Unknown";
    if (by === "machine") return (data.machines.get(key)?.value as Machine | undefined)?.name ?? key;
    if (by === "project") return data.projects.get(key)?.value.name ?? key;
    if (by === "upstream") return upstreamName(key);
    return key;
  };
  const stack = useMemo(() => (report ? stackUsage(report, metric) : null), [report, metric]);
  const colour = useColours(stack?.series.map((s) => s.key) ?? [], by);
  const totals = useMemo(() => usageTotals(report?.rows ?? []), [report]);

  return (
    <div className="space-y-3">
      <div className="flex flex-wrap items-center gap-2" role="group" aria-label="Usage filters">
        <Segmented label="Range" value={range} onChange={setRange} options={RANGES} />
        <Select label="Group usage" className="w-36" value={by} onChange={setBy} options={GROUPS} />
        <Segmented label="Measure" value={metric} onChange={setMetric} options={METRICS} />
      </div>
      {problem && <Problem>{problem}</Problem>}
      <dl className={cn("grid grid-cols-2 gap-2 sm:grid-cols-4", loading && "opacity-60")} aria-label="Totals">
        {[
          ["Estimated cost", usd(totals.cost_micros), "From list prices or yours: an estimate, not a bill."],
          ["Requests", compact(totals.requests), undefined],
          ["Tokens", compact(totals.tokens), `${compact(totals.cache_read_tokens)} read from cache`],
          ["Errors", compact(totals.errors), totals.requests ? `${((totals.errors / totals.requests) * 100).toFixed(1)}% of requests` : undefined],
        ].map(([label, value, hint]) => (
          <div key={label} className="rounded-lg border border-line-2 bg-panel-2 px-3 py-2 inset-shadow-highlight" title={hint}>
            <dt className="text-[11.5px] text-muted">{label}</dt>
            <dd className="text-[17px] font-[590] tracking-[-0.02em]">{value}</dd>
          </div>
        ))}
      </dl>
      {stack && (
        <div className={cn("transition-opacity duration-200 motion-reduce:transition-none", loading && "opacity-50")}>
          <StackedColumns stack={stack} metric={metric} colour={colour} name={name} />
          <ul className="mt-2.5 flex flex-wrap gap-x-4 gap-y-1.5 text-[12px]" aria-label="Legend">
            {stack.series.map((s) => (
              <li key={s.key} className="flex min-w-0 items-center gap-1.5">
                <span className="size-2.5 shrink-0 rounded-[3px]" style={{ background: colour(s.key) }} aria-hidden />
                <span className="truncate text-muted">{name(s.key)}</span>
                <span className="font-mono text-[11.5px] tabular-nums text-fg">{formatMetric(s.total, metric)}</span>
              </li>
            ))}
            {stack.series.length === 0 && <li className="text-subtle">Nothing in this range yet.</li>}
          </ul>
          <button type="button" onClick={() => setTable((t) => !t)} aria-expanded={table} className="mt-2 flex h-6 items-center gap-1.5 rounded-md px-1.5 text-[11.5px] text-subtle hover:bg-fg/[0.06] hover:text-fg">
            <Table2 className="size-3" /> {table ? "Hide the table" : "Show as a table"}
          </button>
          {table && <UsageTable stack={stack} metric={metric} name={name} />}
        </div>
      )}
      {!stack && !problem && (
        <p className="h-40 text-[12.5px] text-subtle">
          <span className="shimmer">Loading usage…</span>
        </p>
      )}
      {report?.truncated && <p className="text-[11.5px] text-subtle">Only the newest rows fit: pick a shorter range for every group.</p>}
    </div>
  );
}

const fmtDay = (t: number, long = false) => new Date(t).toLocaleDateString([], long ? { weekday: "short", month: "short", day: "numeric", timeZone: "UTC" } : { month: "short", day: "numeric", timeZone: "UTC" });

function StackedColumns({ stack, metric, colour, name }: { stack: ReturnType<typeof stackUsage>; metric: UsageMetric; colour: (k: string) => string; name: (k: string) => string }) {
  const wrap = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(560);
  const [hover, setHover] = useState<number | null>(null);
  useEffect(() => {
    const el = wrap.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(([e]) => setWidth(Math.max(240, Math.round(e.contentRect.width))));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const H = 168;
  const left = 44;
  const top = 8;
  const bottom = 20;
  const plotW = width - left;
  const plotH = H - top - bottom;
  const n = stack.at.length;
  const band = plotW / Math.max(1, n);
  const barW = Math.max(2, Math.min(24, band * 0.62));
  const max = niceMax(stack.max);
  const y = (v: number) => top + plotH - (v / max) * plotH;
  const ticks = [0, max / 2, max];
  const labelEvery = Math.ceil(n / Math.max(2, Math.floor(plotW / 64)));
  const at = (i: number) => left + band * i + band / 2;
  const pick = (clientX: number) => {
    const r = wrap.current?.getBoundingClientRect();
    if (!r) return;
    const i = Math.floor((clientX - r.left - left) / band);
    setHover(i >= 0 && i < n ? i : null);
  };
  const summary = `${formatMetric(stack.totals.reduce((a, b) => a + b, 0), metric)} over ${n} days`;
  return (
    <div ref={wrap} className="relative" onPointerMove={(e) => pick(e.clientX)} onPointerLeave={() => setHover(null)}>
      <svg width={width} height={H} role="img" aria-label={`Usage by day: ${summary}`} className="block overflow-visible">
        {ticks.map((t) => (
          <g key={t}>
            <line x1={left} x2={width} y1={y(t)} y2={y(t)} stroke="var(--line)" strokeWidth={1} shapeRendering="crispEdges" />
            <text x={left - 8} y={y(t)} dy="0.32em" textAnchor="end" className="fill-subtle font-mono text-[10.5px] tabular-nums">
              {formatMetric(t, metric)}
            </text>
          </g>
        ))}
        {stack.at.map((t, i) => {
          let cum = 0;
          const x = at(i) - barW / 2;
          const segs = stack.series.map((s) => ({ key: s.key, v: s.values[i] })).filter((s) => s.v > 0);
          const topKey = segs[segs.length - 1]?.key;
          return (
            <g
              key={t}
              tabIndex={0}
              role="button"
              aria-label={`${fmtDay(t, true)}: ${formatMetric(stack.totals[i], metric)}`}
              onFocus={() => setHover(i)}
              onBlur={() => setHover((h) => (h === i ? null : h))}
              className="outline-none"
            >
              {/* The hit target: the whole band, not just the painted bar. */}
              <rect x={left + band * i} y={top} width={band} height={plotH} fill="transparent" />
              {hover === i && <rect x={left + band * i} y={top} width={band} height={plotH} className="fill-fg/[0.04]" />}
              {segs.map((s) => {
                // Bottom up, the biggest first: each segment stops 2px under the next (the surface gap).
                const h = (s.v / max) * plotH;
                const y0 = y(cum + s.v);
                cum += s.v;
                const gap = s.key === topKey ? 0 : 2;
                const height = Math.max(0, h - gap);
                if (height <= 0) return null;
                const r = s.key === topKey ? Math.min(4, barW / 2, height) : 0;
                return <path key={s.key} d={roundedTop(x, y0 + gap, barW, height, r)} fill={colour(s.key)} />;
              })}
            </g>
          );
        })}
        {stack.at.map((t, i) =>
          i % labelEvery === 0 || i === n - 1 ? (
            <text key={t} x={at(i)} y={H - 4} textAnchor="middle" className="fill-subtle text-[10.5px]">
              {fmtDay(t)}
            </text>
          ) : null,
        )}
      </svg>
      {hover !== null && (
        <div
          role="tooltip"
          className="pointer-events-none absolute top-1 z-10 min-w-44 rounded-[10px] border border-outline bg-popover px-2.5 py-2 text-[12px] shadow-pop inset-shadow-highlight"
          style={{ left: Math.min(width - 190, Math.max(0, at(hover) + 12)) }}
        >
          <p className="mb-1 text-[11px] text-subtle">{fmtDay(stack.at[hover], true)}</p>
          <ul className="space-y-0.5">
            {[...stack.series].reverse().map((s) => (
              <li key={s.key} className="flex items-center gap-2">
                <span className="h-[2px] w-2.5 shrink-0 rounded-full" style={{ background: colour(s.key) }} aria-hidden />
                <span className="font-mono text-[11.5px] font-medium tabular-nums text-fg">{formatMetric(s.values[hover], metric)}</span>
                <span className="min-w-0 truncate text-muted">{name(s.key)}</span>
              </li>
            ))}
            <li className="mt-1 flex items-center gap-2 border-t border-line pt-1">
              <span className="w-2.5" />
              <span className="font-mono text-[11.5px] font-medium tabular-nums text-fg">{formatMetric(stack.totals[hover], metric)}</span>
              <span className="text-muted">in all</span>
            </li>
          </ul>
        </div>
      )}
    </div>
  );
}

/** A column segment with its top corners rounded by `r` (square at the bottom). */
function roundedTop(x: number, y: number, w: number, h: number, r: number): string {
  if (r <= 0) return `M${x},${y}h${w}v${h}h${-w}Z`;
  return `M${x},${y + h}V${y + r}Q${x},${y} ${x + r},${y}H${x + w - r}Q${x + w},${y} ${x + w},${y + r}V${y + h}Z`;
}

function UsageTable({ stack, metric, name }: { stack: ReturnType<typeof stackUsage>; metric: UsageMetric; name: (k: string) => string }) {
  return (
    <div className="mt-2 max-h-64 overflow-auto rounded-lg border border-line-2">
      <table className="w-full text-[12px]">
        <thead className="sticky top-0 bg-panel-2 text-left text-[11px] text-muted">
          <tr>
            <th className="px-2.5 py-1.5 font-[510]">Day</th>
            {stack.series.map((s) => (
              <th key={s.key} className="px-2.5 py-1.5 text-right font-[510]">
                {name(s.key)}
              </th>
            ))}
            <th className="px-2.5 py-1.5 text-right font-[510]">All</th>
          </tr>
        </thead>
        <tbody className="divide-y divide-line font-mono tabular-nums">
          {stack.at.map((t, i) => (
            <tr key={t}>
              <td className="px-2.5 py-1 font-sans text-muted">{fmtDay(t, true)}</td>
              {stack.series.map((s) => (
                <td key={s.key} className="px-2.5 py-1 text-right">
                  {formatMetric(s.values[i], metric)}
                </td>
              ))}
              <td className="px-2.5 py-1 text-right text-fg">{formatMetric(stack.totals[i], metric)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
