"use client";

import { useState } from "react";
import { formatBytes, formatNum, formatUsd, qapi, type Usage } from "@/lib/queues-api";
import { usePoll } from "./hooks";
import { QueuesShell } from "./shell";
import { Empty, Panel, Segmented, StatGrid, td, th } from "./ui";

type Metric = "usd" | "messages" | "storage" | "connections";

const METRICS: { value: Metric; label: string }[] = [
  { value: "usd", label: "cost" },
  { value: "messages", label: "messages" },
  { value: "storage", label: "storage" },
  { value: "connections", label: "connections" },
];

const DAYS = ["7", "30", "90"] as const;

/** `day` is the unix second the day starts at (tolerate days-since-epoch too). */
function day(ts: number) {
  const s = ts < 1e6 ? ts * 86400 : ts;
  return new Date(s * 1000).toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC" });
}

export function QueuesUsage() {
  const [days, setDays] = useState<(typeof DAYS)[number]>("30");
  const poll = usePoll(`usage:${days}`, () => qapi.usage(Number(days)), 30_000);
  const u = poll.data;

  return (
    <QueuesShell
      title="Usage"
      crumb="usage"
      description="Metered on messages, GB-months stored and connection-hours. Nothing is billed while a vhost sleeps but its storage."
      loading={poll.loading && !poll.error && !poll.offline}
      offline={poll.offline && !u}
      error={poll.error}
      meta={<Segmented value={days} options={DAYS.map((d) => ({ value: d, label: `${d}d` }))} onChange={setDays} />}
    >
      {u ? <UsageBody u={u} /> : <Empty>{poll.error ? "Usage isn't available." : "Loading usage…"}</Empty>}
    </QueuesShell>
  );
}

function UsageBody({ u }: { u: Usage }) {
  const messages = u.messages.published + u.messages.delivered;
  return (
    <>
      <StatGrid
        stats={[
          { label: `Messages · ${u.period.days}d`, value: formatNum(messages), hint: `${formatNum(u.messages.published)} in · ${formatNum(u.messages.delivered)} out · ${formatUsd(u.messages.usd)}` },
          { label: "Storage", value: `${u.storage.gb_months.toFixed(u.storage.gb_months < 10 ? 3 : 1)} GB-mo`, hint: `${formatBytes(u.storage.current_bytes)} now · ${formatUsd(u.storage.usd)}` },
          { label: "Connection-hours", value: formatNum(Math.round(u.connections.hours * 10) / 10), hint: formatUsd(u.connections.usd) },
          { label: "Projected / month", value: formatUsd(u.projected_month_usd), hint: `${formatUsd(u.total_usd)} so far this period` },
        ]}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <DailyChart u={u} />
        <Panel title="Pricing" className="lg:col-span-2">
          <dl className="divide-y divide-line text-[13px]">
            <PriceRow label="per million messages" value={formatUsd(u.pricing.usd_per_million_messages)} />
            <PriceRow label="per GB-month stored" value={formatUsd(u.pricing.usd_per_gb_month)} />
            <PriceRow label="per connection-hour" value={formatUsd(u.pricing.usd_per_connection_hour)} />
          </dl>
          <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">{u.pricing.basis}</p>
        </Panel>
      </div>
      <Compare u={u} />
    </>
  );
}

function PriceRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between px-5 py-3">
      <dt className="text-muted">{label}</dt>
      <dd className="font-mono tabular-nums">{value}</dd>
    </div>
  );
}

function DailyChart({ u }: { u: Usage }) {
  const [metric, setMetric] = useState<Metric>("usd");
  const value = (d: Usage["daily"][number]) =>
    metric === "usd" ? d.usd : metric === "messages" ? d.published + d.delivered : metric === "storage" ? d.stored_bytes : d.connection_hours;
  const fmt = (n: number) =>
    metric === "usd" ? formatUsd(n) : metric === "storage" ? formatBytes(n) : metric === "connections" ? `${n.toFixed(1)} h` : formatNum(n);
  const max = Math.max(1e-9, ...u.daily.map(value));
  return (
    <Panel title="Daily" actions={<Segmented value={metric} options={METRICS} onChange={setMetric} />} className="lg:col-span-3">
      <div className="flex flex-1 items-end gap-[3px] px-5 pt-8 pb-2" style={{ minHeight: 200 }}>
        {u.daily.length === 0 && <p className="w-full self-center text-center text-[12px] text-subtle">No usage recorded yet.</p>}
        {u.daily.map((d) => (
          <div key={d.day} className="group relative flex h-36 flex-1 items-end">
            <div
              className="w-full rounded-sm bg-fg/15 transition-[height] duration-300 ease-out group-hover:bg-accent motion-reduce:transition-none"
              style={{ height: `${Math.max(1.5, (value(d) / max) * 100)}%` }}
            />
            <span className="pointer-events-none absolute -top-7 left-1/2 z-10 hidden -translate-x-1/2 rounded border border-line bg-panel-2 px-1.5 py-0.5 font-mono text-[10px] whitespace-nowrap group-hover:block">
              {day(d.day)} · {fmt(value(d))}
            </span>
          </div>
        ))}
      </div>
      {u.daily.length > 0 && (
        <div className="flex justify-between px-5 pb-3 font-mono text-[10px] text-subtle">
          <span>{day(u.daily[0].day)}</span>
          <span>{day(u.daily[u.daily.length - 1].day)}</span>
        </div>
      )}
    </Panel>
  );
}

function Compare({ u }: { u: Usage }) {
  const rows = [
    { name: "roll queues", plan: "Pay per use, scales to zero", monthly_usd: u.projected_month_usd, note: "Projected from this period", self: true },
    ...u.compare.map((c) => ({ ...c, self: false })),
  ];
  const max = Math.max(1e-9, ...rows.map((r) => r.monthly_usd));
  const roll = u.projected_month_usd;
  return (
    <Panel title="Same workload elsewhere" meta="monthly, at list prices">
      <div className="overflow-x-auto">
        <table className="w-full text-[13px]">
          <thead>
            <tr>
              <th className={th}>Service</th>
              <th className={th}>Plan</th>
              <th className={`${th} w-1/3`} />
              <th className={`${th} text-right`}>Monthly</th>
              <th className={`${th} text-right`}>vs roll</th>
            </tr>
          </thead>
          <tbody>
            {rows.map((r) => (
              <tr key={r.name + r.plan} className={r.self ? "bg-panel-2/60" : ""}>
                <td className={`${td} whitespace-nowrap ${r.self ? "font-medium" : ""}`}>{r.name}</td>
                <td className={`${td} text-muted`}>
                  <p>{r.plan}</p>
                  {r.note && <p className="text-[11px] text-subtle">{r.note}</p>}
                </td>
                <td className={td}>
                  <div className="h-1.5 overflow-hidden rounded-full bg-line">
                    <div className={`h-full rounded-full ${r.self ? "bg-fg" : "bg-line-2"}`} style={{ width: `${Math.max(1, (r.monthly_usd / max) * 100)}%` }} />
                  </div>
                </td>
                <td className={`${td} text-right font-mono tabular-nums`}>{formatUsd(r.monthly_usd)}</td>
                <td className={`${td} text-right font-mono text-[12px] whitespace-nowrap text-muted tabular-nums`}>
                  {r.self ? "—" : roll > 0 ? `${(r.monthly_usd / roll).toFixed(r.monthly_usd / roll >= 10 ? 0 : 1)}×` : "∞"}
                </td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>
    </Panel>
  );
}
