"use client";

import { DotField, RollMark } from "@roll/brand";
import { ArrowRight } from "lucide-react";
import Link from "next/link";
import {
  type Endpoints,
  formatBytes,
  formatDuration,
  formatNum,
  formatRate,
  qapi,
  type RatePoint,
} from "@/lib/queues-api";
import { useMarkState } from "../mark-state";
import { usePoll } from "./hooks";
import { type ChartSeries, LiveChart } from "./live-chart";
import { QueuesShell } from "./shell";
import { CopyButton, CopyField, ICON, Panel, StatGrid, StateDot } from "./ui";

const RATE_SERIES: ChartSeries<RatePoint>[] = [
  { key: "publish", label: "Publish", tone: "fg", fill: true },
  { key: "deliver", label: "Deliver", tone: "muted" },
  { key: "ack", label: "Ack", tone: "subtle", dashed: true },
];
const DEPTH_SERIES: ChartSeries<RatePoint>[] = [
  { key: "ready", label: "Ready", tone: "fg", fill: true },
  { key: "unacked", label: "Unacked", tone: "muted", dashed: true },
];
const CONN_SERIES: ChartSeries<RatePoint>[] = [{ key: "connections", label: "Connections", tone: "fg", fill: true }];

const PROTOCOLS: { key: keyof Endpoints; label: string; note: string }[] = [
  { key: "amqps", label: "AMQPS", note: "TLS" },
  { key: "mqtt", label: "MQTT", note: "3.1.1 · 5" },
  { key: "stomp", label: "STOMP", note: "1.0–1.2" },
  { key: "stream", label: "Streams", note: "RabbitMQ stream protocol" },
  { key: "http", label: "HTTP", note: "publish · long-poll · SSE" },
  { key: "management", label: "Management", note: "RabbitMQ HTTP API · /metrics" },
];

const count = (n: number) => formatNum(Math.round(n));

export function QueuesOverview() {
  const { state } = useMarkState();
  const overview = usePoll("overview", qapi.overview);
  const rates = usePoll("rates", () => qapi.rates(300));
  const vhosts = usePoll("vhosts", qapi.vhosts, 10_000);
  const o = overview.data;
  const points = rates.data?.points ?? [];
  const amqp = o?.endpoints.amqp ?? vhosts.data?.[0]?.amqp_url;

  return (
    <QueuesShell
      title="Queues"
      description="RabbitMQ-compatible messaging. Same protocols and clients, serverless underneath."
      loading={overview.loading && !overview.error}
      offline={overview.offline && !o}
      error={overview.error ?? rates.error}
      meta={o && `${o.cluster_name} · ${o.node} · v${o.version} · up ${formatDuration(o.uptime_s)}`}
    >
      {o && (
        <>
          <section className="relative isolate overflow-hidden rounded-2xl border border-line bg-panel">
            <DotField spacing={18} radius={110} mode={state === "thinking" || state === "deploying" ? "thinking" : "idle"} className="-z-10 text-fg" />
            <div className="flex items-center gap-8 p-8">
              <div className="min-w-0 flex-1">
                <h2 className="max-w-md text-3xl font-semibold tracking-[-0.045em] text-balance">Point any RabbitMQ client here.</h2>
                <p className="mt-2 max-w-md text-[13px] text-muted">
                  AMQP 0-9-1 and 1.0 on one port. Vhosts sleep when idle and wake on connect.
                </p>
                {amqp && <CopyField value={amqp} className="mt-6 max-w-md bg-bg py-2" />}
              </div>
              <RollMark state={state} size={148} reach={0.9} className="hidden shrink-0 text-fg sm:block" label={`roll queues, ${state}`} />
            </div>
          </section>

          <StatGrid
            stats={[
              { label: "Messages ready", value: count(o.totals.messages_ready), hint: `${count(o.totals.messages_unacked)} unacked` },
              { label: "Publish rate", value: formatRate(o.rates.publish), hint: `deliver ${formatRate(o.rates.deliver)} · ack ${formatRate(o.rates.ack)}` },
              { label: "Connections", value: count(o.totals.connections), hint: `${count(o.totals.channels)} channels · ${count(o.totals.consumers)} consumers` },
              { label: "Stored", value: formatBytes(o.totals.stored_bytes), hint: `${o.storage.backend} · meta ${o.storage.metadata}` },
            ]}
          />

          <div className="grid gap-4 lg:grid-cols-5">
            <Panel title="Message rates" meta="msg/s · 5 min" className="lg:col-span-3">
              <LiveChart points={points} series={RATE_SERIES} height={196} />
            </Panel>
            <Panel title="Topology" className="lg:col-span-2">
              <dl className="grid flex-1 grid-cols-2 gap-px bg-line">
                {[
                  ["Vhosts", o.totals.vhosts, "/dashboard/queues/vhosts"],
                  ["Queues", o.totals.queues, "/dashboard/queues/queues"],
                  ["Exchanges", o.totals.exchanges, "/dashboard/queues/exchanges"],
                  ["Consumers", o.totals.consumers, "/dashboard/queues/connections"],
                  ["Connections", o.totals.connections, "/dashboard/queues/connections"],
                  ["Channels", o.totals.channels, "/dashboard/queues/connections"],
                ].map(([label, n, href]) => (
                  <Link key={label as string} href={href as string} className="group bg-panel px-5 py-4 transition hover:bg-panel-2">
                    <dt className="flex items-center justify-between text-[12px] text-subtle">
                      {label}
                      <ArrowRight {...ICON} className="size-3 opacity-0 transition group-hover:opacity-100" />
                    </dt>
                    <dd className="mt-1 font-mono text-xl font-semibold tracking-[-0.03em] tabular-nums">{count(n as number)}</dd>
                  </Link>
                ))}
              </dl>
            </Panel>
          </div>

          <div className="grid gap-4 lg:grid-cols-2">
            <Panel title="Queue depth" meta="messages">
              <LiveChart points={points} series={DEPTH_SERIES} format={count} live={false} whole />
            </Panel>
            <Panel title="Connections" meta="open">
              <LiveChart points={points} series={CONN_SERIES} format={count} live={false} whole />
            </Panel>
          </div>

          <div className="grid gap-4 lg:grid-cols-5">
            <Panel title="Endpoints" className="lg:col-span-3">
              <ul>
                {o.endpoints.amqp && <EndpointRow label="AMQP" note="0-9-1 · 1.0" value={o.endpoints.amqp} />}
                {PROTOCOLS.map((p) => {
                  const v = o.endpoints[p.key];
                  return v ? <EndpointRow key={p.key} label={p.label} note={p.note} value={v} /> : null;
                })}
              </ul>
            </Panel>
            <Panel
              title="Vhosts"
              meta={vhosts.data ? `${vhosts.data.filter((v) => v.state === "running").length} awake` : undefined}
              actions={
                <Link href="/dashboard/queues/vhosts" className="text-[12px] text-accent hover:underline">
                  Manage
                </Link>
              }
              className="lg:col-span-2"
            >
              <ul>
                {(vhosts.data ?? []).slice(0, 6).map((v) => (
                  <li key={v.name} className="flex items-center gap-3 border-b border-line px-5 py-2.5 last:border-0">
                    <StateDot state={v.state === "running" ? "running" : "sleeping"} />
                    <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{v.name}</span>
                    <span className="font-mono text-[11px] text-subtle tabular-nums">
                      {count(v.messages_ready)} ready · {v.connections} conn
                    </span>
                  </li>
                ))}
              </ul>
            </Panel>
          </div>
        </>
      )}
    </QueuesShell>
  );
}

function EndpointRow({ label, note, value }: { label: string; note: string; value: string }) {
  return (
    <li className="flex items-center gap-3 border-b border-line px-5 py-2.5 last:border-0">
      <span className="w-24 shrink-0 text-[13px]">{label}</span>
      <span className="hidden w-40 shrink-0 truncate text-[11px] text-subtle md:inline">{note}</span>
      <code className="min-w-0 flex-1 truncate font-mono text-[12px] text-muted">{value}</code>
      <CopyButton text={value} />
    </li>
  );
}
