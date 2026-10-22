"use client";

import { TriangleAlert } from "lucide-react";
import { useEffect, useState, type ReactNode } from "react";
import { formatBytes, formatUsd } from "@/lib/roll-api";
import { formatCount } from "@/lib/redis-api";
import { formatSeconds, nosqlApi, type NosqlActivity, type NosqlDb, type NosqlInfo, type NosqlUsage } from "@/lib/nosql-api";
import { Panel, Stats } from "../ui";
import { POLL_MS, Segmented } from "./shared";

/** Dollars, keeping fractions of a cent visible: scale-to-zero bills are often tiny. */
function usd(n: number) {
  if (n === 0) return "$0";
  if (n < 0.0001) return "<$0.0001";
  return formatUsd(n);
}

function exact(n: number) {
  return `$${n < 1 ? n.toPrecision(3) : n.toFixed(2)}`;
}

export function UsageTab({ db, info }: { db: NosqlDb; info: NosqlInfo }) {
  const [days, setDays] = useState(30);
  const [usage, setUsage] = useState<NosqlUsage | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () =>
      nosqlApi.usage(db.id, days).then(
        (u) => {
          if (!alive) return;
          setUsage(u);
          setErr(null);
        },
        (e) => alive && setErr(e instanceof Error ? e.message : String(e)),
      );
    load();
    const t = setInterval(load, POLL_MS * 8);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [db.id, days]);

  // Hold the previous period's numbers, dimmed, while the new one loads (no layout jump).
  const stale = usage != null && usage.days !== days;
  const u = usage;
  const c = u?.cost;
  const p = info.pricing;
  const period = days === 1 ? "24 hours" : `${days} days`;

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="text-[13px] text-muted">
          Metered every minute while loaded. A scaled-to-zero {db.parent_id ? "branch" : "database"} pays only for the
          SSTables it owns.
        </p>
        <div className="w-48">
          <Segmented
            value={days}
            options={[
              { label: "24h", value: 1 },
              { label: "7d", value: 7 },
              { label: "30d", value: 30 },
            ]}
            onChange={setDays}
          />
        </div>
      </div>
      {err && <p className="text-[13px] text-danger">{err}</p>}
      <div className={`space-y-4 transition-opacity duration-200 ${stale ? "opacity-60" : ""}`}>
        <Stats
          items={[
            {
              label: "Storage",
              value: u ? formatBytes(u.storage_bytes) : "—",
              hint: db.parent_id ? "owned by this branch (shared SSTables are the parent's)" : "SSTables on object storage",
            },
            {
              label: "Requests",
              value: u ? formatCount(u.requests) : "—",
              hint: u ? `${formatCount(u.reads)} reads · ${formatCount(u.writes)} writes` : undefined,
            },
            {
              label: "Connection time",
              value: u ? formatSeconds(u.connection_seconds) : "—",
              hint: u ? `${formatBytes(u.net_in_bytes)} in · ${formatBytes(u.net_out_bytes)} out` : undefined,
            },
            {
              label: "Loaded",
              value: u ? formatSeconds(u.active_seconds) : "—",
              hint: u ? `${((u.active_seconds / (u.days * 86_400)) * 100).toFixed(1)}% of the period` : undefined,
            },
          ]}
        />
        <div className="grid gap-4 lg:grid-cols-5">
          <Panel
            title="Cost estimate"
            aside={<span className="font-mono text-[11px] text-subtle">last {period} · AWS list prices</span>}
            className="lg:col-span-3"
          >
            <div className="px-5 py-4">
              <CostRows
                rows={
                  c
                    ? [
                        { label: "Loaded time", detail: `${p.usd_per_active_hour} USD per hour loaded`, usd: c.compute_usd },
                        { label: "Reads", detail: `${p.usd_per_million_reads} USD per million`, usd: c.read_usd },
                        { label: "Writes", detail: `${p.usd_per_million_writes} USD per million`, usd: c.write_usd },
                        { label: "Storage", detail: `S3 at ${p.usd_per_gb_month_s3} USD per GB-month`, usd: c.storage_usd },
                        { label: "Egress", detail: `${p.usd_per_gb_out} USD per GB out`, usd: c.egress_usd },
                      ]
                    : []
                }
                total={c?.total_usd ?? 0}
              />
            </div>
          </Panel>
          <Panel title="Same workload elsewhere" className="lg:col-span-2">
            <div className="space-y-4 px-5 py-4">
              {c ? (
                <Compare
                  rows={[
                    { label: "roll", usd: c.total_usd, basis: "metered usage above", emphasis: true },
                    {
                      label: "Amazon Keyspaces",
                      usd: c.keyspaces_usd,
                      basis: `on-demand: ${p.keyspaces_usd_per_million_writes}/M writes, ${p.keyspaces_usd_per_million_reads}/M reads, ${p.keyspaces_usd_per_gb_month}/GB-month`,
                    },
                    {
                      label: "DynamoDB",
                      usd: c.dynamodb_usd,
                      basis: `on-demand: ${p.dynamodb_usd_per_million_writes}/M writes, ${p.dynamodb_usd_per_million_reads}/M reads, ${p.dynamodb_usd_per_gb_month}/GB-month`,
                    },
                    {
                      label: "Astra DB Serverless",
                      usd: c.astra_usd,
                      basis: `${p.astra_usd_per_million_writes}/M writes, ${p.astra_usd_per_million_reads}/M reads, ${p.astra_usd_per_gb_month}/GB-month`,
                    },
                    {
                      label: "ScyllaDB Cloud (floor)",
                      usd: c.scylla_cloud_usd,
                      basis: `${p.scylla_cloud_min_nodes} nodes × ${p.scylla_cloud_usd_per_node_hour} USD/hour, always on`,
                    },
                  ]}
                />
              ) : (
                <p className="text-[13px] text-subtle">Loading…</p>
              )}
              <p className="text-[11px] text-subtle">
                Each request counted as one request unit (rows up to 1 KB written, 4 KB read), with the same storage and
                egress. ScyllaDB Cloud publishes no hourly price: the floor is its minimum three-node cluster at EC2 list
                prices, without the service fee. us-east-1, checked 2026-10.
              </p>
            </div>
          </Panel>
        </div>
      </div>
    </div>
  );
}

/** Hover / focus tooltip for one mark (CSS only, 150ms fade). */
function Tip({ children }: { children: ReactNode }) {
  return (
    <span
      role="tooltip"
      className="pointer-events-none absolute bottom-full left-0 z-10 mb-1.5 w-max max-w-64 rounded-md border border-line-2 bg-panel px-2.5 py-1.5 text-[11px] text-muted opacity-0 transition-opacity duration-150 group-hover:opacity-100 group-focus-visible:opacity-100 motion-reduce:transition-none"
    >
      {children}
    </span>
  );
}

/** Cost breakdown: one neutral series; bars carry magnitude, text carries identity and value. */
function CostRows({ rows, total }: { rows: { label: string; detail: string; usd: number }[]; total: number }) {
  if (rows.length === 0) return <p className="text-[13px] text-subtle">Loading…</p>;
  const max = Math.max(...rows.map((r) => r.usd), 1e-12);
  return (
    <table className="w-full text-[13px]">
      <tbody>
        {rows.map((r) => (
          <tr key={r.label} className="border-b border-line">
            <td className="py-2.5 pr-3 align-top">
              <p>{r.label}</p>
              <p className="text-[11px] text-subtle">{r.detail}</p>
            </td>
            <td className="hidden w-2/5 py-2.5 pr-3 align-middle sm:table-cell">
              <div tabIndex={0} className="group relative flex h-6 items-center border-l border-line-2 outline-none" aria-label={`${r.label}: ${exact(r.usd)}`}>
                <div
                  className="h-2 rounded-r-[4px] bg-fg/30 transition-[width,background-color] duration-300 group-hover:bg-fg/60 group-focus-visible:bg-fg/60 motion-reduce:transition-none"
                  style={{ width: `${r.usd > 0 ? Math.max(1.5, (r.usd / max) * 100) : 0}%` }}
                />
                <Tip>
                  <span className="text-fg">{r.label}</span> · <span className="font-mono">{exact(r.usd)}</span>
                  <br />
                  {r.detail}
                </Tip>
              </div>
            </td>
            <td className="py-2.5 text-right align-top font-mono tabular-nums">{usd(r.usd)}</td>
          </tr>
        ))}
        <tr>
          <td className="pt-3 font-medium">Total</td>
          <td className="hidden sm:table-cell" />
          <td className="pt-3 text-right font-mono font-semibold tabular-nums">{usd(total)}</td>
        </tr>
      </tbody>
    </table>
  );
}

/** Emphasis: roll in the accent, the alternatives in neutral ink; every value is printed. */
function Compare({ rows }: { rows: { label: string; usd: number; basis: string; emphasis?: boolean }[] }) {
  const max = Math.max(...rows.map((r) => r.usd), 1e-12);
  const ours = rows.find((r) => r.emphasis)?.usd ?? 0;
  return (
    <div className="space-y-3">
      {rows.map((r) => {
        const times = !r.emphasis && ours > 0 && r.usd > ours ? r.usd / ours : null;
        return (
          <div key={r.label} tabIndex={0} className="group relative outline-none" aria-label={`${r.label}: ${exact(r.usd)}. ${r.basis}`}>
            <div className="flex items-baseline justify-between gap-3 text-[12px]">
              <span className={r.emphasis ? "text-fg" : "text-muted"}>{r.label}</span>
              <span className="font-mono tabular-nums">
                {usd(r.usd)}
                {times != null && times >= 1.5 && (
                  <span className="ml-2 text-[11px] text-subtle">{times >= 1000 ? ">1,000×" : times >= 100 ? `${Math.round(times)}×` : `${times.toFixed(1)}×`}</span>
                )}
              </span>
            </div>
            <div className="mt-1.5 flex h-2.5 items-center border-l border-line-2">
              <div
                className={`h-2 rounded-r-[4px] transition-[width,background-color] duration-300 motion-reduce:transition-none ${
                  r.emphasis ? "bg-accent" : "bg-fg/30 group-hover:bg-fg/60 group-focus-visible:bg-fg/60"
                }`}
                style={{ width: `${r.usd > 0 ? Math.max(1, (r.usd / max) * 100) : 0}%` }}
              />
            </div>
            <Tip>
              <span className="text-fg">{r.label}</span> · <span className="font-mono">{exact(r.usd)}</span>
              <br />
              {r.basis}
            </Tip>
          </div>
        );
      })}
    </div>
  );
}

// ---------------------------------------------------------------------------------------------
// Activity

const CLIENT_COLUMNS = ["address", "username", "driver_name", "driver_version", "protocol_version", "keyspace", "ssl_enabled", "request_count"];

export function ActivityTab({ db }: { db: NosqlDb }) {
  const [a, setA] = useState<NosqlActivity | null>(null);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    // Reading activity never loads the database.
    const loop = async () => {
      try {
        const v = await nosqlApi.activity(db.id);
        if (alive) {
          setA(v);
          setErr(null);
        }
      } catch (e) {
        if (alive) setErr(e instanceof Error ? e.message : String(e));
      }
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [db.id]);

  if (err && !a) return <p className="text-[13px] text-danger">{err}</p>;
  if (!a) {
    return (
      <p className="text-[13px] text-subtle">
        <span className="shimmer">Loading…</span>
      </p>
    );
  }
  const m = a.metrics;
  const idle = a.state === "idle";
  return (
    <div className="space-y-4">
      <Stats
        items={[
          {
            label: "State",
            value: idle ? "scaled to zero" : "loaded",
            hint: idle ? "no clients; counters since start" : `${a.users ?? 0} in use · idle ${formatSeconds(Math.floor((a.idle_ms ?? 0) / 1000))}`,
          },
          { label: "Clients", value: formatCount(m.connections), hint: `${formatCount(m.connections_total)} connections · ${formatCount(m.loads)} loads` },
          { label: "Requests", value: formatCount(m.requests), hint: `${formatCount(m.http_requests)} over HTTP · ${formatCount(m.errors)} errors` },
          { label: "Traffic", value: formatBytes(m.bytes_out), hint: `out · ${formatBytes(m.bytes_in)} in` },
        ]}
      />
      <Panel title="Connected clients" aside={<span className="font-mono text-[12px] text-subtle">{a.clients.length}</span>}>
        {a.clients.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">
            {idle ? "Scaled to zero: nobody is connected." : "No client connections are registered right now."}
          </p>
        ) : (
          <RecordTable records={a.clients} prefer={CLIENT_COLUMNS} />
        )}
      </Panel>
      <div className="grid gap-4 lg:grid-cols-2">
        <Panel title="Slow queries" aside={<span className="font-mono text-[12px] text-subtle">{a.slow_queries.length}</span>}>
          {a.slow_queries.length === 0 ? (
            <p className="px-5 py-6 text-[13px] text-muted">
              None recorded.{" "}
              {db.dialect === "scylla" ? "ScyllaDB's slow query log (system_traces.node_slow_log) is empty." : "Cassandra keeps no slow query log table."}
            </p>
          ) : (
            <RecordTable records={a.slow_queries} prefer={["start_time", "duration", "parameters", "table_names", "source_ip", "username"]} />
          )}
        </Panel>
        <Panel title="Warnings" aside={<span className="font-mono text-[12px] text-subtle">{a.warnings.length}</span>}>
          {a.warnings.length === 0 ? (
            <p className="px-5 py-6 text-[13px] text-muted">
              No tombstone or batch-size warnings. Statements that trigger one show it in the console too.
            </p>
          ) : (
            <ul className="divide-y divide-line">
              {a.warnings.map((w, i) => (
                <li key={i} className="flex gap-2 px-5 py-2.5 text-[12px] text-muted">
                  <TriangleAlert className="mt-0.5 size-3.5 shrink-0 text-subtle" aria-label="Warning" />
                  <span className="min-w-0 font-mono break-words">{typeof w === "string" ? w : JSON.stringify(w)}</span>
                </li>
              ))}
            </ul>
          )}
        </Panel>
      </div>
    </div>
  );
}

function cell(v: unknown) {
  if (v == null) return <span className="text-subtle italic">null</span>;
  if (typeof v === "object") return JSON.stringify(v);
  return String(v);
}

/** Rows of JSON objects as a table: preferred columns first, at most eight. */
function RecordTable({ records, prefer }: { records: Record<string, unknown>[]; prefer: string[] }) {
  const keys = new Set(records.flatMap((r) => Object.keys(r)));
  const cols = [...prefer.filter((k) => keys.has(k)), ...[...keys].filter((k) => !prefer.includes(k)).sort()].slice(0, 8);
  return (
    <div className="overflow-x-auto">
      <table className="w-full border-collapse font-mono text-[12px]">
        <thead>
          <tr className="border-b border-line text-left text-[11px] text-subtle">
            {cols.map((c) => (
              <th key={c} className="px-3 py-2 font-normal whitespace-nowrap first:pl-5 last:pr-5">
                {c}
              </th>
            ))}
          </tr>
        </thead>
        <tbody>
          {records.map((r, i) => (
            <tr key={i} className="border-b border-line last:border-0">
              {cols.map((c) => (
                <td key={c} className="max-w-64 truncate px-3 py-1.5 first:pl-5 last:pr-5" title={typeof r[c] === "object" ? JSON.stringify(r[c]) : String(r[c] ?? "")}>
                  {cell(r[c])}
                </td>
              ))}
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  );
}
