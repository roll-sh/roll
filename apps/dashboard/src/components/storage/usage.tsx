"use client";

import { useEffect, useRef, useState } from "react";
import { formatCount } from "@/lib/redis-api";
import { formatBytes } from "@/lib/roll-api";
import { storageApi, usd, type StorageCost, type StorageInfo, type StorageUsage } from "@/lib/storage-api";
import { Panel, Stats } from "../ui";
import { Segmented } from "./shared";

type Metric = "usd" | "requests" | "storage" | "egress";
const METRICS: { value: Metric; label: string }[] = [
  { value: "usd", label: "cost" },
  { value: "requests", label: "requests" },
  { value: "storage", label: "storage" },
  { value: "egress", label: "egress" },
];
const PERIODS = [
  { label: "7d", value: 7 },
  { label: "30d", value: 30 },
  { label: "90d", value: 90 },
];

type Day = StorageUsage["daily"][number];

function dayLabel(day: string) {
  const d = new Date(`${day}T00:00:00Z`);
  return Number.isNaN(d.getTime()) ? day : d.toLocaleDateString([], { month: "short", day: "numeric", timeZone: "UTC" });
}

/** Load now and every 30 s; reloads when `key` changes. */
function usePolled<T>(key: string, load: () => Promise<T>) {
  const [data, setData] = useState<T | null>(null);
  const [err, setErr] = useState<string | null>(null);
  const loadRef = useRef(load);
  loadRef.current = load;
  useEffect(() => {
    let alive = true;
    const run = () =>
      loadRef.current().then(
        (d) => {
          if (!alive) return;
          setData(d);
          setErr(null);
        },
        (e) => alive && setErr(e instanceof Error ? e.message : String(e)),
      );
    run();
    const t = setInterval(run, 30_000);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [key]);
  return { data, err };
}

/** Every bucket: totals, daily chart, per-bucket cost table. */
export function UsageTab({ info }: { info: StorageInfo }) {
  const [days, setDays] = useState(30);
  const { data, err } = usePolled(`all:${days}`, () => storageApi.usageAll(days));
  const u = data?.days === days ? data : null;
  const rows = u ? [...u.buckets].sort((a, b) => b.cost.total_usd - a.cost.total_usd) : [];
  const sum = (f: (b: StorageUsage) => number) => rows.reduce((s, b) => s + f(b), 0);

  return (
    <div className="space-y-4">
      <PeriodBar days={days} onChange={setDays} />
      {err && <p className="text-[13px] text-danger">{err}</p>}
      <UsageStats
        cost={u?.cost ?? null}
        classA={sum((b) => b.class_a)}
        classB={sum((b) => b.class_b)}
        gbMonths={sum((b) => b.gb_months)}
        stored={sum((b) => b.storage_bytes)}
        egressInternet={sum((b) => b.egress_internet_bytes)}
        egressInternal={sum((b) => b.egress_internal_bytes)}
        egressCached={sum((b) => b.egress_cached_bytes ?? 0)}
        loaded={!!u}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <DailyChart daily={u?.daily ?? []} loaded={!!u} className="lg:col-span-3" />
        <PricingPanel info={info} className="lg:col-span-2" />
      </div>
      <Panel title="By bucket" aside={<span className="font-mono text-[11px] text-subtle">last {days} days</span>}>
        {!u ? (
          <p className="px-5 py-6 text-[13px] text-subtle">
            <span className="shimmer">Loading usage…</span>
          </p>
        ) : rows.length === 0 ? (
          <p className="px-5 py-6 text-[13px] text-muted">No usage recorded yet.</p>
        ) : (
          <div className="overflow-x-auto">
            <table className="w-full text-[13px]">
              <thead>
                <tr className="border-b border-line text-left text-[11px] text-subtle">
                  <th className="px-5 py-2 font-normal">Bucket</th>
                  <th className="px-3 py-2 text-right font-normal">Stored</th>
                  <th className="px-3 py-2 text-right font-normal">Writes</th>
                  <th className="px-3 py-2 text-right font-normal">Reads</th>
                  <th className="px-3 py-2 text-right font-normal">Egress</th>
                  <th className="px-5 py-2 text-right font-normal">Cost</th>
                </tr>
              </thead>
              <tbody>
                {rows.map((b) => (
                  <tr key={b.bucket} className="border-b border-line last:border-0">
                    <td className="px-5 py-2.5 font-mono text-[12px]">{b.bucket}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatBytes(b.storage_bytes)}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatCount(b.class_a)}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">{formatCount(b.class_b)}</td>
                    <td className="px-3 py-2.5 text-right font-mono text-[12px] tabular-nums">
                      {formatBytes(b.egress_internet_bytes)}
                    </td>
                    <td className="px-5 py-2.5 text-right font-mono tabular-nums">{usd(b.cost.total_usd)}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          </div>
        )}
      </Panel>
    </div>
  );
}

/** One bucket's usage. */
export function BucketUsage({ info, bucket }: { info: StorageInfo; bucket: string }) {
  const [days, setDays] = useState(30);
  const { data, err } = usePolled(`${bucket}:${days}`, () => storageApi.usage(bucket, days));
  const u = data?.days === days ? data : null;
  return (
    <div className="space-y-4">
      <PeriodBar days={days} onChange={setDays} />
      {err && <p className="text-[13px] text-danger">{err}</p>}
      <UsageStats
        cost={u?.cost ?? null}
        classA={u?.class_a ?? 0}
        classB={u?.class_b ?? 0}
        gbMonths={u?.gb_months ?? 0}
        stored={u?.storage_bytes ?? 0}
        egressInternet={u?.egress_internet_bytes ?? 0}
        egressInternal={u?.egress_internal_bytes ?? 0}
        egressCached={u?.egress_cached_bytes ?? 0}
        loaded={!!u}
      />
      <div className="grid gap-4 lg:grid-cols-5">
        <DailyChart daily={u?.daily ?? []} loaded={!!u} className="lg:col-span-3" />
        <CostPanel info={info} cost={u?.cost ?? null} days={days} className="lg:col-span-2" />
      </div>
    </div>
  );
}

function PeriodBar({ days, onChange }: { days: number; onChange: (d: number) => void }) {
  return (
    <div className="flex flex-wrap items-center justify-between gap-3">
      <p className="text-[13px] text-muted">
        Metered every minute at S3 list prices. Deletes are free; traffic inside roll is free.
      </p>
      <div className="w-48">
        <Segmented value={days} options={PERIODS} onChange={onChange} />
      </div>
    </div>
  );
}

function UsageStats({
  cost,
  classA,
  classB,
  gbMonths,
  stored,
  egressInternet,
  egressInternal,
  egressCached,
  loaded,
}: {
  cost: StorageCost | null;
  classA: number;
  classB: number;
  gbMonths: number;
  stored: number;
  egressInternet: number;
  egressInternal: number;
  egressCached: number;
  loaded: boolean;
}) {
  return (
    <Stats
      items={[
        {
          label: "Storage",
          value: loaded ? `${gbMonths.toFixed(gbMonths < 10 ? 3 : 1)} GB-mo` : "—",
          hint: loaded ? `${formatBytes(stored)} now` : undefined,
        },
        {
          label: "Requests",
          value: loaded ? formatCount(classA + classB) : "—",
          hint: loaded ? `${formatCount(classA)} writes · ${formatCount(classB)} reads` : undefined,
        },
        {
          label: "Egress",
          value: loaded ? formatBytes(egressInternet + egressCached) : "—",
          hint: loaded
            ? `${formatBytes(egressCached)} via CDN · ${formatBytes(egressInternal)} inside roll, free`
            : undefined,
        },
        {
          label: "Cost",
          value: cost ? usd(cost.total_usd) : "—",
          hint: cost ? `${usd(cost.projected_month_usd)} projected / month` : undefined,
        },
      ]}
    />
  );
}

function DailyChart({ daily, loaded, className = "" }: { daily: Day[]; loaded: boolean; className?: string }) {
  const [metric, setMetric] = useState<Metric>("usd");
  const value = (d: Day) =>
    metric === "usd" ? d.usd : metric === "requests" ? d.class_a + d.class_b : metric === "storage" ? d.storage_bytes : d.egress_bytes;
  const fmt = (n: number) => (metric === "usd" ? usd(n) : metric === "requests" ? formatCount(n) : formatBytes(n));
  const max = Math.max(1e-9, ...daily.map(value));
  return (
    <Panel
      title="Daily"
      aside={
        <div className="w-72 max-w-full">
          <Segmented value={metric} options={METRICS} onChange={setMetric} />
        </div>
      }
      className={className}
    >
      <div className="flex flex-1 items-end gap-[3px] px-5 pt-8 pb-2" style={{ minHeight: 200 }}>
        {daily.length === 0 && (
          <p className="w-full self-center text-center text-[12px] text-subtle">
            {loaded ? "No usage recorded yet." : <span className="shimmer">Loading…</span>}
          </p>
        )}
        {daily.map((d) => (
          <div key={d.day} className="group relative flex h-36 flex-1 items-end">
            <div
              className="w-full rounded-sm bg-fg/15 transition-[height] duration-300 ease-out group-hover:bg-accent motion-reduce:transition-none"
              style={{ height: `${Math.max(1.5, (value(d) / max) * 100)}%` }}
            />
            <span className="pointer-events-none absolute -top-7 left-1/2 z-10 hidden -translate-x-1/2 rounded border border-line bg-panel-2 px-1.5 py-0.5 font-mono text-[10px] whitespace-nowrap group-hover:block">
              {dayLabel(d.day)} · {fmt(value(d))}
            </span>
          </div>
        ))}
      </div>
      {daily.length > 0 && (
        <div className="flex justify-between px-5 pb-3 font-mono text-[10px] text-subtle">
          <span>{dayLabel(daily[0].day)}</span>
          <span>{dayLabel(daily[daily.length - 1].day)}</span>
        </div>
      )}
    </Panel>
  );
}

function PricingPanel({ info, className = "" }: { info: StorageInfo; className?: string }) {
  const p = info.pricing;
  return (
    <Panel title="Pricing" className={className}>
      <dl className="divide-y divide-line text-[13px]">
        <PriceRow label="per GB-month stored" value={usd(p.usd_per_gb_month)} />
        <PriceRow label="per million writes (class A)" value={usd(p.usd_per_million_class_a)} />
        <PriceRow label="per million reads (class B)" value={usd(p.usd_per_million_class_b)} />
        <PriceRow label="per GB egress to the internet" value={usd(p.usd_per_gb_egress_internet)} />
        <PriceRow label="per GB through the CDN" value={usd(p.usd_per_gb_egress_cached ?? 0)} />
        <PriceRow label="per GB inside roll" value={usd(p.usd_per_gb_egress_internal)} />
      </dl>
      <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">
        S3 Standard list prices (us-east-1). DELETE and aborting uploads are free.
      </p>
    </Panel>
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

/** Cost breakdown: one neutral series, so bars carry magnitude and text carries identity. */
function CostPanel({ info, cost, days, className = "" }: { info: StorageInfo; cost: StorageCost | null; days: number; className?: string }) {
  const p = info.pricing;
  const rows = cost
    ? [
        { label: "Storage", detail: `${usd(p.usd_per_gb_month)} / GB-month`, usd: cost.storage_usd },
        { label: "Writes", detail: `${usd(p.usd_per_million_class_a)} / million`, usd: cost.class_a_usd },
        { label: "Reads", detail: `${usd(p.usd_per_million_class_b)} / million`, usd: cost.class_b_usd },
        { label: "Egress", detail: `${usd(p.usd_per_gb_egress_internet)} / GB to the internet`, usd: cost.egress_usd },
        { label: "CDN egress", detail: `${usd(p.usd_per_gb_egress_cached ?? 0)} / GB served by the CDN`, usd: cost.egress_cached_usd ?? 0 },
      ]
    : [];
  const max = Math.max(1e-9, ...rows.map((r) => r.usd));
  return (
    <Panel title="Cost" aside={<span className="font-mono text-[11px] text-subtle">last {days} days</span>} className={className}>
      <div className="px-5 py-4">
        {rows.length === 0 ? (
          <p className="text-[13px] text-subtle">
            <span className="shimmer">Loading…</span>
          </p>
        ) : (
          <table className="w-full text-[13px]">
            <tbody>
              {rows.map((r) => (
                <tr key={r.label} className="group border-b border-line">
                  <td className="py-2.5 pr-3 align-top">
                    <p>{r.label}</p>
                    <p className="text-[11px] text-subtle">{r.detail}</p>
                  </td>
                  <td className="hidden w-2/5 py-2.5 pr-3 align-middle sm:table-cell">
                    <div className="h-1.5 rounded-full bg-line">
                      <div
                        className="h-full rounded-full bg-fg/30 transition-[width] duration-300 group-hover:bg-fg/60 motion-reduce:transition-none"
                        style={{ width: `${r.usd > 0 ? Math.max(2, (r.usd / max) * 100) : 0}%` }}
                      />
                    </div>
                  </td>
                  <td className="py-2.5 text-right align-top font-mono tabular-nums">{usd(r.usd)}</td>
                </tr>
              ))}
              <tr>
                <td className="pt-3 font-medium">Total</td>
                <td className="hidden sm:table-cell" />
                <td className="pt-3 text-right font-mono font-semibold tabular-nums">{usd(cost?.total_usd ?? 0)}</td>
              </tr>
            </tbody>
          </table>
        )}
      </div>
    </Panel>
  );
}
