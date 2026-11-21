import { useEffect, useMemo, useState } from "react";
import { appsApi, formatMs, PRODUCTION, type Metrics, type Service } from "@/lib/apps-api";
import type { WorkspaceProject } from "@/lib/workspace-api";
import { input, panel } from "../ui";
import { Link } from "@/components/link";

const RANGES = [
  { label: "Last hour", minutes: 60 },
  { label: "Last day", minutes: 1440 },
];

/** One service's row: what its own Metrics tab reads, summed over the range. */
export interface Row {
  id: string;
  name: string;
  requests: number;
  /** Share of requests answered 5xx, 0–100. */
  errorPct: number;
  /** Its worst minute's p95 (ms), when it answered any. */
  p95: number | null;
  /** Error groups seen in the range that aren't resolved or muted. */
  openErrors: number;
  series: number[];
  /** Why it couldn't be read (its own metrics answered an error). */
  failed?: string;
}

/** A service's row from its metrics and its errors. */
export function rowOf(s: Pick<Service, "id" | "name">, m: Metrics, groups: { state?: string }[]): Row {
  const requests = m.requests.reduce((a, b) => a + b, 0);
  const errors = m.errors.reduce((a, b) => a + b, 0);
  const p95s = m.p95_ms.filter((v, i) => v > 0 && m.requests[i] > 0);
  return {
    id: s.id,
    name: s.name,
    requests,
    errorPct: requests ? (errors * 100) / requests : 0,
    p95: p95s.length ? Math.max(...p95s) : null,
    openErrors: groups.filter((g) => (g.state ?? "open") === "open").length,
    series: m.requests,
  };
}

/** The worst first: errors, then open error groups, then the slowest p95. */
export function worstFirst(rows: Row[]): Row[] {
  return [...rows].sort((a, b) => b.errorPct - a.errorPct || b.openErrors - a.openErrors || (b.p95 ?? 0) - (a.p95 ?? 0) || a.name.localeCompare(b.name));
}

/**
 * A project's services side by side in one environment (Railway's Observability, Vercel's): each
 * one's requests, 5xx share, worst p95 and open error groups, the worst first. Each row reads its
 * service's own Metrics and Errors routes, so nothing new reads them and each read is allowed as
 * its tab's is.
 */
export function ProjectMetrics({ project }: { project: WorkspaceProject }) {
  const [services, setServices] = useState<Service[] | null>(null);
  const [envName, setEnvName] = useState(PRODUCTION);
  const [range, setRange] = useState(RANGES[0]);
  const [rows, setRows] = useState<Row[] | null>(null);

  useEffect(() => {
    appsApi.services(project.id).then(setServices, () => setServices([]));
  }, [project.id]);
  const envs = useMemo(() => [...new Set((services ?? []).flatMap((s) => s.environments.map((e) => e.name)))].sort((a, b) => (a === PRODUCTION ? -1 : b === PRODUCTION ? 1 : a.localeCompare(b))), [services]);
  const shown = useMemo(() => (services ?? []).filter((s) => s.run.type === "web" && s.environments.some((e) => e.name === envName)), [services, envName]);

  useEffect(() => {
    let live = true;
    setRows(null);
    void Promise.all(
      shown.map(async (s) => {
        try {
          const [m, e] = await Promise.all([appsApi.metrics(s.id, envName, range.minutes), appsApi.errors(s.id, envName, range.minutes).catch(() => ({ groups: [] }))]);
          return rowOf(s, m, e.groups);
        } catch (e) {
          return { id: s.id, name: s.name, requests: 0, errorPct: 0, p95: null, openErrors: 0, series: [], failed: e instanceof Error ? e.message : String(e) } satisfies Row;
        }
      }),
    ).then((r) => live && setRows(worstFirst(r)));
    return () => {
      live = false;
    };
  }, [shown, envName, range]);

  return (
    <div className="mx-auto max-w-5xl space-y-4 px-6 py-6">
      <div className="flex flex-wrap items-center gap-2">
        <select value={envName} onChange={(e) => setEnvName(e.target.value)} aria-label="Environment" className={`${input} !w-auto text-[12px]`}>
          {(envs.length ? envs : [PRODUCTION]).map((e) => (
            <option key={e}>{e}</option>
          ))}
        </select>
        <select value={range.minutes} onChange={(e) => setRange(RANGES.find((r) => r.minutes === Number(e.target.value)) ?? RANGES[0])} aria-label="Range" className={`${input} !w-auto text-[12px]`}>
          {RANGES.map((r) => (
            <option key={r.minutes} value={r.minutes}>
              {r.label}
            </option>
          ))}
        </select>
        <span className="text-[12px] text-subtle">its web services, the worst first</span>
      </div>
      <section className={`${panel} overflow-hidden`} aria-label="Services">
        {rows === null ? (
          <p className="px-4 py-3 text-[13px] text-muted">{services === null ? "Reading its services…" : "Reading their metrics…"}</p>
        ) : rows.length === 0 ? (
          <p className="px-4 py-3 text-[13px] text-muted">No web service runs in {envName}.</p>
        ) : (
          <table className="w-full text-[12.5px]">
            <thead>
              <tr className="border-b border-line text-left text-[11.5px] text-subtle">
                <th className="px-4 py-2 font-normal">Service</th>
                <th className="px-3 py-2 text-right font-normal">Requests</th>
                <th className="px-3 py-2 text-right font-normal">5xx</th>
                <th className="px-3 py-2 text-right font-normal">Worst p95</th>
                <th className="px-3 py-2 text-right font-normal">Open errors</th>
                <th className="px-4 py-2 font-normal">Requests over time</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((r) => (
                <tr key={r.id} className="border-b border-line last:border-0">
                  <td className="px-4 py-2">
                    <Link href={`/dashboard/apps/${encodeURIComponent(r.id)}?env=${encodeURIComponent(envName)}&tab=metrics`} className="font-medium hover:text-accent">
                      {r.name}
                    </Link>
                    {r.failed && <span className="ml-2 text-[11.5px] text-danger">{r.failed}</span>}
                  </td>
                  <td className="px-3 py-2 text-right font-mono">{r.requests.toLocaleString()}</td>
                  <td className={`px-3 py-2 text-right font-mono ${r.errorPct >= 1 ? "text-danger" : "text-muted"}`}>{r.requests ? `${r.errorPct.toFixed(r.errorPct < 10 ? 1 : 0)}%` : "–"}</td>
                  <td className="px-3 py-2 text-right font-mono text-muted">{r.p95 === null ? "–" : formatMs(r.p95)}</td>
                  <td className={`px-3 py-2 text-right font-mono ${r.openErrors ? "text-fg" : "text-subtle"}`}>{r.openErrors || "–"}</td>
                  <td className="px-4 py-2">
                    <Spark values={r.series} />
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        )}
      </section>
    </div>
  );
}

/** A small line of a series (no axes: its row says the numbers). */
function Spark({ values }: { values: number[] }) {
  if (values.length < 2) return <span className="text-subtle">–</span>;
  const max = Math.max(1, ...values);
  const w = 120;
  const pts = values.map((v, i) => `${((i / (values.length - 1)) * w).toFixed(1)},${(18 - (v / max) * 16).toFixed(1)}`).join(" ");
  return (
    <svg width={w} height={20} viewBox={`0 0 ${w} 20`} aria-hidden className="text-accent">
      <polyline points={pts} fill="none" stroke="currentColor" strokeWidth={1.25} />
    </svg>
  );
}
