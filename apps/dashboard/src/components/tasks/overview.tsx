"use client";

import { Send } from "lucide-react";
import { useMemo, useState } from "react";
import { formatUsd } from "@/lib/roll-api";
import {
  formatCount,
  rel,
  tasks,
  type Stats,
  type UsageCounters,
  type UsageReport,
} from "@/lib/tasks-api";
import type { Tab } from "./tasks";
import {
  Chip,
  Empty,
  IdLink,
  input,
  Loading,
  Panel,
  parseJsonInput,
  primary,
  Segmented,
  StateDot,
  useActions,
  useNow,
  usePoll,
} from "./ui";

const METRICS: { key: keyof UsageCounters; label: string }[] = [
  { key: "published", label: "Published" },
  { key: "deliveries", label: "Deliveries" },
  { key: "delivered", label: "Delivered" },
  { key: "failed_attempts", label: "Failed attempts" },
  { key: "dead", label: "Dead" },
  { key: "events", label: "Events" },
  { key: "runs", label: "Runs" },
  { key: "steps", label: "Steps" },
];

const RANGES = [
  { value: "7", label: "7d" },
  { value: "14", label: "14d" },
  { value: "30", label: "30d" },
] as const;

/** A worker counts as live if it heartbeated recently. */
const LIVE_MS = 30_000;

export function OverviewTab({ go }: { go: (t: Tab) => void }) {
  const [days, setDays] = useState<"7" | "14" | "30">("14");
  const stats = usePoll(() => tasks.stats(), 4000);
  const usage = usePoll(() => tasks.usage(Number(days)), 15_000, days);

  return (
    <div className="space-y-4">
      <Headline stats={stats.data} usage={usage.data} go={go} />
      <div className="grid gap-4 lg:grid-cols-5">
        <ThroughputChart usage={usage.data} days={days} setDays={setDays} />
        <CostPanel usage={usage.data} />
      </div>
      <div className="grid gap-4 lg:grid-cols-5">
        <Backlog stats={stats.data} go={go} />
        <Workers stats={stats.data} />
      </div>
      <TestMessage queues={stats.data?.queues.map((q) => q.name).filter((n) => !n.startsWith("fn:")) ?? []} onSent={stats.reload} />
    </div>
  );
}

function Headline({ stats, usage, go }: { stats: Stats | null; usage: UsageReport | null; go: (t: Tab) => void }) {
  const t = usage?.total;
  const backlog = stats?.queues.reduce((s, q) => s + q.stats.ready + q.stats.scheduled, 0) ?? null;
  const successRate = t && t.deliveries > 0 ? t.delivered / t.deliveries : null;
  const items = [
    {
      label: `Published · ${usage?.period.days ?? "—"}d`,
      value: t ? formatCount(t.published) : "—",
      hint: t ? `${formatCount(t.events)} events · ${formatCount(t.runs)} runs` : " ",
    },
    {
      label: "Delivered",
      value: t ? formatCount(t.delivered) : "—",
      hint: successRate != null ? `${(successRate * 100).toFixed(1)}% of ${formatCount(t!.deliveries)} attempts` : "no deliveries yet",
    },
    {
      label: "Dead letters",
      value: stats ? `${stats.dlq}${stats.dlq_more ? "+" : ""}` : "—",
      hint: stats && stats.dlq > 0 ? "retries exhausted — review" : "nothing failed for good",
      onClick: () => go("dlq"),
      warn: (stats?.dlq ?? 0) > 0,
    },
    {
      label: "Backlog",
      value: backlog != null ? formatCount(backlog) : "—",
      hint: stats ? `${stats.queues.length} queues · ${stats.schedules} schedules` : " ",
      onClick: () => go("queues"),
    },
  ];
  return (
    <div className="grid grid-cols-2 gap-px overflow-hidden rounded-2xl border border-line bg-line lg:grid-cols-4">
      {items.map((s) => (
        <button
          key={s.label}
          onClick={s.onClick}
          disabled={!s.onClick}
          className="bg-panel p-5 text-left transition enabled:hover:bg-panel-2"
        >
          <p className="text-[12px] text-subtle">{s.label}</p>
          <p className={`mt-2 text-2xl font-semibold tracking-[-0.03em] tabular-nums ${s.warn ? "text-danger" : ""}`}>
            {s.value}
          </p>
          <p className="mt-1 truncate text-[12px] text-muted">{s.hint}</p>
        </button>
      ))}
    </div>
  );
}

function dayLabel(day: number) {
  return new Date(day * 86_400_000).toLocaleDateString(undefined, { month: "short", day: "numeric", timeZone: "UTC" });
}

function ThroughputChart({
  usage,
  days,
  setDays,
}: {
  usage: UsageReport | null;
  days: "7" | "14" | "30";
  setDays: (d: "7" | "14" | "30") => void;
}) {
  const [metric, setMetric] = useState<keyof UsageCounters>("published");
  // The API only returns days with activity; fill the gaps.
  const series = useMemo(() => {
    if (!usage) return Array.from({ length: Number(days) }, (_, i) => ({ day: i, v: 0, empty: true }));
    const byDay = new Map(usage.daily.map((d) => [d.day, d.usage]));
    const out = [];
    for (let d = usage.period.start_day; d <= usage.period.end_day; d++) {
      out.push({ day: d, v: byDay.get(d)?.[metric] ?? 0, empty: false });
    }
    return out;
  }, [usage, metric, days]);
  const max = Math.max(1, ...series.map((s) => s.v));
  const total = usage?.total[metric] ?? 0;

  return (
    <Panel
      className="lg:col-span-3"
      title="Throughput"
      actions={
        <Segmented value={days} options={RANGES.map((r) => ({ value: r.value, label: r.label }))} onChange={setDays} />
      }
    >
      <div className="flex flex-wrap gap-x-3 gap-y-1 px-5 pt-3">
        {METRICS.map((m) => (
          <button
            key={m.key}
            onClick={() => setMetric(m.key)}
            className={`text-[12px] transition ${metric === m.key ? "text-fg" : "text-subtle hover:text-muted"}`}
          >
            {m.label}
            <span className="ml-1 font-mono text-[10px] tabular-nums">{usage ? formatCount(usage.total[m.key]) : ""}</span>
          </button>
        ))}
      </div>
      <div className="flex flex-1 items-end gap-[3px] px-5 pt-8 pb-2" style={{ minHeight: 170 }}>
        {series.map((d) => (
          <div key={d.day} className="group relative flex h-32 flex-1 items-end">
            <div
              className={`w-full rounded-sm transition-[height] duration-300 group-hover:bg-accent ${
                d.v > 0 ? "bg-fg/25" : "bg-fg/8"
              }`}
              style={{ height: `${d.v > 0 ? Math.max(4, (d.v / max) * 100) : 2}%` }}
            />
            {!d.empty && (
              <span className="pointer-events-none absolute -top-7 left-1/2 z-10 hidden -translate-x-1/2 rounded border border-line bg-panel-2 px-1.5 py-0.5 font-mono text-[10px] whitespace-nowrap group-hover:block">
                {dayLabel(d.day)} · {d.v.toLocaleString()}
              </span>
            )}
          </div>
        ))}
      </div>
      <div className="flex justify-between px-5 pb-3 font-mono text-[10px] text-subtle">
        <span>{usage ? dayLabel(usage.period.start_day) : ""}</span>
        <span>{usage ? "today (UTC)" : ""}</span>
      </div>
      <p className="border-t border-line px-5 py-3 text-[12px] text-subtle">
        {usage
          ? `${total.toLocaleString()} ${METRICS.find((m) => m.key === metric)!.label.toLowerCase()} in ${usage.period.days} days · ${formatBytesShort(usage.total.bytes_in)} in, ${formatBytesShort(usage.total.bytes_out)} out`
          : " "}
      </p>
    </Panel>
  );
}

function formatBytesShort(n: number) {
  if (n < 1024) return `${n} B`;
  if (n < 1024 ** 2) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024 ** 3).toFixed(2)} GB`;
}

function CostPanel({ usage }: { usage: UsageReport | null }) {
  const c = usage?.cost;
  const fixed = c?.fixed_usd_per_month ?? 0;
  const rows = c ? c.items.map((i) => [i.name, i.usd] as const) : [];
  return (
    <Panel
      className="lg:col-span-2"
      title="AWS cost estimate"
      meta={c ? `${c.layout === "log" ? "S3 log store" : "DynamoDB"} · us-east-1` : "us-east-1 list prices"}
    >
      <div className="space-y-4 px-5 py-4">
        <div>
          <p className="text-[12px] text-subtle">Projected / month</p>
          <p className="mt-1 text-2xl font-semibold tracking-[-0.03em] tabular-nums">
            {usage ? formatUsd(usage.projected_month_usd + fixed) : "—"}
          </p>
          <p className="mt-0.5 text-[12px] text-muted">
            {usage && c ? `${formatUsd(usage.projected_month_usd)} usage + ${formatUsd(fixed)} for ${c.fixed}` : " "}
          </p>
        </div>
        <div className="grid grid-cols-2 gap-3">
          <div className="rounded-lg border border-line px-3 py-2.5">
            <p className="text-[11px] text-subtle">Per million messages</p>
            <p className="mt-0.5 font-mono text-[14px] tabular-nums">{c ? formatUsd(c.usd_per_million_messages) : "—"}</p>
          </div>
          <div className="rounded-lg border border-line px-3 py-2.5">
            <p className="text-[11px] text-subtle">Always on / month</p>
            <p className="mt-0.5 font-mono text-[14px] tabular-nums">{c ? formatUsd(fixed) : "—"}</p>
          </div>
        </div>
        <div>
          <p className="mb-1.5 text-[12px] text-subtle">This period · {usage ? `${usage.period.days}d` : "—"}</p>
          <ul className="space-y-1 font-mono text-[12px]">
            {rows.map(([k, v]) => (
              <li key={k} className="flex justify-between">
                <span className="text-muted">{k}</span>
                <span className="tabular-nums">{formatUsd(v)}</span>
              </li>
            ))}
            <li className="flex justify-between border-t border-line pt-1">
              <span className="text-muted">Total</span>
              <span className="tabular-nums">{c ? formatUsd(c.total_usd) : "—"}</span>
            </li>
          </ul>
        </div>
      </div>
      <p className="mt-auto border-t border-line px-5 py-3 text-[12px] text-subtle">
        {c?.layout === "dynamodb"
          ? "Lambda API, Fargate Spot workers, DynamoDB on-demand. Everything but the workers scales to zero."
          : c?.hosted
            ? "A tenant of a shared cell: you pay for your share of its S3 commits, not for its nodes."
            : `${c?.front_door === "nlb" ? "Load balancer" : "Lambda proxy"}, a two-node Fargate cell, the log in S3. S3 bills commits, not messages: busier is cheaper per message.`}
      </p>
    </Panel>
  );
}

function Backlog({ stats, go }: { stats: Stats | null; go: (t: Tab) => void }) {
  const queues = stats?.queues ?? [];
  const max = Math.max(1, ...queues.map((q) => q.stats.ready + q.stats.scheduled + q.stats.active));
  return (
    <Panel className="lg:col-span-3" title="Queue backlog" meta={stats ? `${queues.length}` : undefined}>
      {!stats ? (
        <Loading />
      ) : queues.length === 0 ? (
        <Empty title="No queues yet">Messages without a queue go straight to the shared lanes.</Empty>
      ) : (
        <ul>
          {queues.map((q) => {
            const s = q.stats;
            const total = s.ready + s.scheduled + s.active;
            return (
              <li
                key={q.name}
                onClick={() => go("queues")}
                className="flex cursor-pointer items-center gap-3 border-b border-line px-5 py-2.5 transition last:border-0 hover:bg-panel-2/60"
              >
                <StateDot state={q.paused ? "paused" : s.active > 0 ? "live" : total > 0 ? "pending" : "delivered"} />
                <span className="w-32 shrink-0 truncate font-mono text-[12px]">{q.name}</span>
                <div className="flex h-1.5 min-w-0 flex-1 overflow-hidden rounded-full bg-line">
                  <div className="h-full bg-accent transition-[width] duration-500" style={{ width: `${(s.active / max) * 100}%` }} />
                  <div className="h-full bg-fg/40 transition-[width] duration-500" style={{ width: `${(s.ready / max) * 100}%` }} />
                  <div className="h-full bg-fg/20 transition-[width] duration-500" style={{ width: `${(s.scheduled / max) * 100}%` }} />
                </div>
                <span className="shrink-0 text-right font-mono text-[11px] whitespace-nowrap text-subtle tabular-nums">
                  {s.active} active · {s.ready} ready · {s.scheduled} later
                </span>
                {q.paused && <Chip>paused</Chip>}
              </li>
            );
          })}
        </ul>
      )}
    </Panel>
  );
}

function Workers({ stats }: { stats: Stats | null }) {
  const now = useNow(2000);
  const nodes = stats?.nodes ?? [];
  return (
    <Panel className="lg:col-span-2" title="Workers" meta={stats ? `${nodes.length} live` : undefined}>
      {!stats ? (
        <Loading />
      ) : nodes.length === 0 ? (
        <Empty title="No workers">Start rolld (or roll-tasks with --role worker) to deliver messages.</Empty>
      ) : (
        <ul>
          {nodes.map((n) => (
            <li key={n.id} className="flex items-center gap-3 border-b border-line px-5 py-3 last:border-0">
              <StateDot state={now - n.seen_at < LIVE_MS ? (n.inflight > 0 ? "live" : "delivered") : "canceled"} title={`seen ${rel(n.seen_at, now)}`} />
              <div className="min-w-0 flex-1">
                <p className="truncate font-mono text-[12px]">{n.id}</p>
                <p className="text-[11px] text-subtle">
                  up {rel(n.started_at, now).replace(" ago", "")} · seen {rel(n.seen_at, now)}
                </p>
              </div>
              <div className="text-right font-mono text-[11px] tabular-nums">
                <p>{n.inflight} in flight</p>
                <p className="text-subtle">{n.lanes} lanes</p>
              </div>
            </li>
          ))}
        </ul>
      )}
    </Panel>
  );
}

function TestMessage({ queues, onSent }: { queues: string[]; onSent: () => void }) {
  const { run, busy } = useActions();
  const [url, setUrl] = useState("https://httpbin.org/post");
  const [body, setBody] = useState('{\n  "hello": "roll"\n}');
  const [delay, setDelay] = useState("");
  const [retries, setRetries] = useState("3");
  const [queue, setQueue] = useState("");
  const [sent, setSent] = useState<string | null>(null);
  const sending = busy === "test-message";

  const submit = async () => {
    setSent(null);
    const out = await run("test-message", async () => {
      let parsed: unknown;
      try {
        parsed = parseJsonInput(body, "body");
      } catch {
        parsed = body; // not JSON: send it as text
      }
      return tasks.publish({
        url: url.trim(),
        body: parsed,
        delay: delay.trim() || undefined,
        retries: retries.trim() === "" ? undefined : Number(retries),
        queue: queue || undefined,
      });
    });
    if (out) {
      setSent(out.message_id);
      onSent();
    }
  };

  return (
    <Panel title="Send a test message" meta="POST /v1/tasks/publish">
      <form
        className="grid gap-3 px-5 py-4 lg:grid-cols-5"
        onSubmit={(e) => {
          e.preventDefault();
          if (url.trim()) submit();
        }}
      >
        <div className="space-y-3 lg:col-span-3">
          <label className="block space-y-1">
            <span className="block text-[11px] text-subtle">Destination URL</span>
            <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://example.com/api/webhook" className={`${input} w-full font-mono text-[12px]`} />
          </label>
          <label className="block space-y-1">
            <span className="block text-[11px] text-subtle">Body (JSON or text)</span>
            <textarea
              value={body}
              onChange={(e) => setBody(e.target.value)}
              rows={4}
              spellCheck={false}
              className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed`}
            />
          </label>
        </div>
        <div className="flex flex-col gap-3 lg:col-span-2">
          <div className="grid grid-cols-3 gap-2">
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Delay</span>
              <input value={delay} onChange={(e) => setDelay(e.target.value)} placeholder="0s" className={`${input} w-full font-mono text-[12px]`} />
            </label>
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Retries</span>
              <input value={retries} onChange={(e) => setRetries(e.target.value.replace(/\D/g, ""))} inputMode="numeric" className={`${input} w-full font-mono text-[12px]`} />
            </label>
            <label className="space-y-1">
              <span className="block text-[11px] text-subtle">Queue</span>
              <select value={queue} onChange={(e) => setQueue(e.target.value)} className={`${input} w-full text-[12px]`}>
                <option value="">none</option>
                {queues.map((q) => (
                  <option key={q} value={q}>
                    {q}
                  </option>
                ))}
              </select>
            </label>
          </div>
          <p className="text-[12px] text-subtle">
            Delivered as a signed POST. Non-2xx responses and timeouts retry with exponential backoff, then land in dead
            letters.
          </p>
          <div className="mt-auto flex items-center justify-between gap-3">
            <span className="min-w-0 truncate text-[12px] text-muted">
              {sent && (
                <span className="rise inline-flex items-center gap-1.5">
                  Queued <IdLink kind="message" id={sent} />
                </span>
              )}
            </span>
            <button type="submit" disabled={sending || !url.trim()} className={primary}>
              {sending ? (
                <span className="shimmer">Publishing…</span>
              ) : (
                <>
                  <Send className="size-3.5" strokeWidth={1.5} /> Publish
                </>
              )}
            </button>
          </div>
        </div>
      </form>
    </Panel>
  );
}
