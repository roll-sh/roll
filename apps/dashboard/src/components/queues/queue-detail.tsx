"use client";

import { ArrowLeft, Eraser, Link2, RotateCcw, Trash2, Unlink } from "lucide-react";
import Link from "next/link";
import { useParams, useRouter } from "next/navigation";
import { useEffect, useState } from "react";
import {
  type Binding,
  decodeParam,
  exchangeLabel,
  formatBytes,
  formatNum,
  formatRate,
  qapi,
  type QueueDetail as QueueDetailT,
} from "@/lib/queues-api";
import { useAction, usePoll, useSamples } from "./hooks";
import { type ChartSeries, LiveChart } from "./live-chart";
import { MessageBrowser } from "./message-browser";
import { PublishForm } from "./publish-form";
import { QueueBadges } from "./queue-list";
import { QueuesShell } from "./shell";
import {
  Badge,
  btn,
  Busy,
  dangerIconBtn,
  Empty,
  ErrorNote,
  Field,
  ICON,
  input,
  JsonInline,
  num,
  Panel,
  Pending,
  StatGrid,
  StateDot,
  td,
  th,
} from "./ui";

interface Sample {
  t: number;
  ready: number;
  unacked: number;
  publish: number;
  deliver: number;
  ack: number;
}

const DEPTH: ChartSeries<Sample>[] = [
  { key: "ready", label: "Ready", tone: "fg", fill: true },
  { key: "unacked", label: "Unacked", tone: "muted", dashed: true },
];
const RATES: ChartSeries<Sample>[] = [
  { key: "publish", label: "Publish", tone: "fg", fill: true },
  { key: "deliver", label: "Deliver", tone: "muted" },
  { key: "ack", label: "Ack", tone: "subtle", dashed: true },
];

export function QueueDetail() {
  const params = useParams<{ vhost: string; name: string }>();
  const vhost = decodeParam(params.vhost);
  const name = decodeParam(params.name);
  const router = useRouter();
  const poll = usePoll(`queue:${vhost}\u0000${name}`, async () => ({ q: await qapi.queue(vhost, name), t: Date.now() / 1000 }));
  const { busy, error, setError, run } = useAction();
  const [notice, setNotice] = useState<string | null>(null);
  const q = poll.data?.q ?? null;

  const sample: Sample | null = poll.data
    ? {
        t: poll.data.t,
        ready: poll.data.q.messages_ready,
        unacked: poll.data.q.messages_unacked,
        publish: poll.data.q.rates.publish,
        deliver: poll.data.q.rates.deliver,
        ack: poll.data.q.rates.ack,
      }
    : null;
  const [history, setHistory] = useState<Sample[] | null>(null);
  useEffect(() => {
    let live = true;
    qapi
      .queueRates(vhost, name)
      .then((r) => live && setHistory(r.points))
      .catch(() => {});
    return () => {
      live = false;
    };
  }, [vhost, name]);
  const samples = useSamples(sample, 300, history);

  const purge = async () => {
    if (!q || !confirm(`Purge all ${q.messages_ready.toLocaleString("en-US")} ready messages from ${q.name}?`)) return;
    const before = poll.data;
    poll.mutate((d) => ({ ...d, q: { ...d.q, messages_ready: 0, messages: d.q.messages_unacked } }));
    const out = await run("purge", () => qapi.purge(vhost, name), { rollback: () => before && poll.mutate(() => before) });
    if (out) setNotice(`Purged ${out.purged.toLocaleString("en-US")} message${out.purged === 1 ? "" : "s"}`);
    poll.refresh();
  };

  const remove = async () => {
    if (!q || !confirm(`Delete queue ${q.name}${q.messages ? ` and its ${q.messages.toLocaleString("en-US")} messages` : ""}?`)) return;
    const out = await run("delete", () => qapi.deleteQueue(vhost, name));
    if (out !== undefined) router.push(`/dashboard/queues/queues?vhost=${encodeURIComponent(vhost)}`);
  };

  const notFound = poll.error && !q;

  return (
    <QueuesShell
      title={<span className="font-mono tracking-[-0.03em]">{name}</span>}
      crumb={name}
      description={
        q ? (
          <span className="inline-flex flex-wrap items-center gap-1.5">
            <span className="font-mono">vhost {vhost}</span>
            <span className="text-subtle">·</span>
            <span>{q.type}</span>
            <QueueBadges q={q} />
          </span>
        ) : (
          <span className="font-mono">vhost {vhost}</span>
        )
      }
      loading={poll.loading && !poll.error}
      offline={poll.offline && !q}
      error={notFound ? null : poll.error}
      actions={
        q && (
          <div className="flex items-center gap-1.5">
            <button onClick={purge} disabled={busy !== null || q.type === "stream"} className={btn} title={q.type === "stream" ? "Streams can't be purged" : "Remove all ready messages"}>
              {busy === "purge" ? <Busy /> : <Eraser {...ICON} />} Purge
            </button>
            <button onClick={remove} disabled={busy !== null} className={`${btn} hover:text-danger`}>
              {busy === "delete" ? <Busy /> : <Trash2 {...ICON} />} Delete
            </button>
          </div>
        )
      }
    >
      <Link href="/dashboard/queues/queues" className="inline-flex items-center gap-1.5 text-[12px] text-subtle hover:text-fg">
        <ArrowLeft {...ICON} className="size-3" /> All queues
      </Link>
      {notFound && <ErrorNote error={poll.error} />}
      <ErrorNote error={error} onClose={() => setError(null)} />
      {notice && (
        <p className="rise flex items-center justify-between rounded-lg border border-line bg-panel px-3 py-2 text-[13px]">
          {notice}
          <button onClick={() => setNotice(null)} className="text-[12px] text-subtle hover:text-fg">
            Dismiss
          </button>
        </p>
      )}
      {q && (
        <>
          <StatGrid
            stats={[
              { label: "Ready", value: formatNum(q.messages_ready), hint: `${formatNum(q.messages)} total` },
              { label: "Unacked", value: formatNum(q.messages_unacked), hint: `redeliver ${formatRate(q.rates.redeliver)}` },
              { label: "Consumers", value: formatNum(q.consumers), hint: q.consumers ? `${q.consumers === 1 ? "1 consumer" : `${q.consumers} consumers`} attached` : "none attached" },
              { label: "Size", value: formatBytes(q.bytes), hint: q.type === "stream" && q.next_offset !== undefined ? `offsets ${q.first_offset ?? 0}–${q.next_offset}` : `${q.durable ? "durable" : "transient"}` },
            ]}
          />

          <div className="grid gap-4 lg:grid-cols-2">
            <Panel title="Depth" meta="sampled every 5s">
              <LiveChart points={samples} series={DEPTH} format={(n) => formatNum(Math.round(n))} live={false} whole height={150} empty="Sampling depth…" />
            </Panel>
            <Panel title="Rates" meta="msg/s">
              <LiveChart points={samples} series={RATES} height={150} empty="Sampling rates…" />
            </Panel>
          </div>

          {q.is_dlq && <ReplayPanel vhost={vhost} q={q} onDone={() => poll.refresh()} />}

          <Panel title="Messages" meta="browse">
            <MessageBrowser key={`${vhost}\u0000${name}`} vhost={vhost} queue={name} total={q.messages} />
          </Panel>

          <div className="grid gap-4 lg:grid-cols-5">
            <Panel title="Publish" meta="via the default exchange" className="lg:col-span-3">
              <PublishForm vhost={vhost} exchange="" routingKey={name} onPublished={() => poll.refresh()} />
            </Panel>
            <Panel title="Details" className="lg:col-span-2">
              <dl className="divide-y divide-line text-[12px]">
                <Row label="Type" value={q.type} />
                <Row label="Durable" value={q.durable ? "yes" : "no"} />
                <Row label="Exclusive" value={q.exclusive ? "yes" : "no"} />
                <Row label="Auto-delete" value={q.auto_delete ? "yes" : "no"} />
                <Row label="Policy" value={q.policy ?? "—"} />
                <Row label="Dead-letter exchange" value={q.dead_letter_exchange === null ? "—" : exchangeLabel(q.dead_letter_exchange)} />
                <Row label="Dead-lettered" value={formatNum(q.dead_lettered)} />
                {q.type === "stream" && <Row label="Offsets" value={`${q.first_offset ?? "—"} – ${q.next_offset ?? "—"}`} />}
                <div className="px-5 py-2.5">
                  <dt className="mb-1 text-subtle">Arguments</dt>
                  <dd className="break-words">
                    <JsonInline value={q.arguments} />
                  </dd>
                </div>
              </dl>
            </Panel>
          </div>

          <div className="grid gap-4 lg:grid-cols-2">
            <Bindings vhost={vhost} queue={name} bindings={q.bindings} onChange={() => poll.refresh()} />
            <Consumers q={q} />
          </div>
        </>
      )}
    </QueuesShell>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between gap-3 px-5 py-2.5">
      <dt className="text-subtle">{label}</dt>
      <dd className="truncate font-mono">{value}</dd>
    </div>
  );
}

function Consumers({ q }: { q: QueueDetailT }) {
  return (
    <Panel title="Consumers" meta={q.consumer_details.length ? String(q.consumer_details.length) : undefined}>
      {q.consumer_details.length === 0 ? (
        <Empty>No consumers attached.</Empty>
      ) : (
        <div className="overflow-x-auto">
          <table className="w-full text-[12px]">
            <thead>
              <tr>
                <th className={th}>Tag</th>
                <th className={th}>Channel</th>
                <th className={`${th} text-right`}>Prefetch</th>
                <th className={th}>Ack</th>
              </tr>
            </thead>
            <tbody>
              {q.consumer_details.map((c) => (
                <tr key={`${c.channel}-${c.consumer_tag}`}>
                  <td className={`${td} max-w-0 w-full`}>
                    <span className="flex items-center gap-2">
                      <StateDot state={c.active ? "running" : "off"} />
                      <span className="truncate font-mono text-[11px]">{c.consumer_tag}</span>
                      {c.exclusive && <Badge>exclusive</Badge>}
                      {c.priority !== 0 && <Badge>p{c.priority}</Badge>}
                    </span>
                  </td>
                  <td className={`${td} ${num} text-muted`}>
                    {c.connection}/{c.channel}
                  </td>
                  <td className={`${td} ${num} text-right`}>{c.prefetch || "∞"}</td>
                  <td className={`${td} text-muted`}>{c.ack_required ? "manual" : "auto"}</td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      )}
    </Panel>
  );
}

function Bindings({ vhost, queue, bindings, onChange }: { vhost: string; queue: string; bindings: Binding[]; onChange: () => void }) {
  const { busy, error, setError, run } = useAction();
  const [exchanges, setExchanges] = useState<string[] | null>(null);
  const [source, setSource] = useState("");
  const [key, setKey] = useState("");
  const [hidden, setHidden] = useState<Set<string>>(new Set());

  useEffect(() => {
    let alive = true;
    qapi.exchanges(vhost).then(
      (xs) => {
        if (!alive) return;
        const names = xs.map((x) => x.name).filter((n) => n !== "" && !n.startsWith("amq.rabbitmq"));
        setExchanges(names);
        setSource((s) => s || names.find((n) => !n.startsWith("amq.")) || names[0] || "");
      },
      () => alive && setExchanges([]),
    );
    return () => {
      alive = false;
    };
  }, [vhost]);

  const id = (b: Binding) => `${b.source}\u0000${b.routing_key}\u0000${JSON.stringify(b.arguments)}`;
  const shown = bindings.filter((b) => !hidden.has(id(b)));

  const unbind = (b: Binding) => {
    setHidden((h) => new Set(h).add(id(b)));
    run(
      `unbind-${id(b)}`,
      async () => {
        await qapi.unbind(vhost, { source: b.source, destination: queue, destination_type: "queue", routing_key: b.routing_key, arguments: b.arguments });
        onChange();
      },
      {
        rollback: () =>
          setHidden((h) => {
            const n = new Set(h);
            n.delete(id(b));
            return n;
          }),
      },
    );
  };

  return (
    <Panel title="Bindings" meta={String(shown.length)}>
      {shown.length === 0 ? (
        <Empty>No bindings.</Empty>
      ) : (
        <ul>
          {shown.map((b) => (
            <li key={id(b)} className="flex items-center gap-3 border-b border-line px-5 py-2.5 last:border-0">
              <span className="min-w-0 flex-1 truncate font-mono text-[12px]">
                <span className={b.source === "" ? "text-subtle" : ""}>{exchangeLabel(b.source)}</span>
                <span className="text-subtle"> → </span>
                {b.routing_key || <span className="text-subtle">(empty key)</span>}
              </span>
              {Object.keys(b.arguments ?? {}).length > 0 && <JsonInline value={b.arguments} />}
              {b.source !== "" && (
                <button className={dangerIconBtn} disabled={busy !== null} onClick={() => unbind(b)} title="Unbind">
                  <Unlink {...ICON} />
                </button>
              )}
            </li>
          ))}
        </ul>
      )}
      <form
        className="mt-auto flex flex-wrap items-center gap-2 border-t border-line px-5 py-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!source) return;
          const ok = await run("bind", () => qapi.bind(vhost, { source, destination: queue, destination_type: "queue", routing_key: key }));
          if (ok !== undefined) {
            setKey("");
            onChange();
          }
        }}
      >
        <Link2 {...ICON} className="size-3.5 text-subtle" />
        <select value={source} onChange={(e) => setSource(e.target.value)} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} aria-label="Exchange">
          {exchanges === null && <option value="">loading…</option>}
          {exchanges?.length === 0 && <option value="">no exchanges</option>}
          {exchanges?.map((x) => (
            <option key={x} value={x}>
              {x}
            </option>
          ))}
        </select>
        <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="routing key" spellCheck={false} className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <button type="submit" disabled={busy !== null || !source} className={btn}>
          {busy === "bind" ? <Pending>Binding…</Pending> : "Bind"}
        </button>
      </form>
      {error && (
        <div className="px-5 pb-3">
          <ErrorNote error={error} onClose={() => setError(null)} />
        </div>
      )}
    </Panel>
  );
}

function ReplayPanel({ vhost, q, onDone }: { vhost: string; q: QueueDetailT; onDone: () => void }) {
  const { busy, error, setError, run } = useAction();
  const [count, setCount] = useState("");
  const [exchange, setExchange] = useState("");
  const [key, setKey] = useState("");
  const [result, setResult] = useState<{ replayed: number; skipped?: number } | null>(null);

  return (
    <Panel title="Replay dead letters" meta={`${formatNum(q.messages_ready)} waiting`}>
      <form
        className="space-y-3 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          if (count && !/^\d+$/.test(count)) return setError("Count must be a whole number");
          setResult(null);
          const out = await run("replay", () =>
            qapi.replay(vhost, q.name, {
              count: count ? Number(count) : undefined,
              target_exchange: exchange.trim() || undefined,
              target_routing_key: key.trim() || undefined,
            }),
          );
          if (out) {
            setResult(out);
            onDone();
          }
        }}
      >
        <p className="text-[13px] text-muted">
          Re-publish messages to the exchange and routing key they were dead-lettered from (their{" "}
          <span className="font-mono">x-death</span> history), or to a target you choose. Replayed messages leave this
          queue.
        </p>
        <div className="grid gap-2 sm:grid-cols-[8rem_1fr_1fr_auto] sm:items-end">
          <Field label="Count">
            <input value={count} onChange={(e) => setCount(e.target.value)} placeholder="all" inputMode="numeric" className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Target exchange">
            <input value={exchange} onChange={(e) => setExchange(e.target.value)} placeholder="from x-death" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <Field label="Target routing key">
            <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="from x-death" spellCheck={false} className={`${input} w-full font-mono text-[12px]`} />
          </Field>
          <button type="submit" disabled={busy !== null || q.messages_ready === 0} className={btn}>
            {busy === "replay" ? (
              <Pending>Replaying…</Pending>
            ) : (
              <>
                <RotateCcw {...ICON} /> Replay
              </>
            )}
          </button>
        </div>
        <ErrorNote error={error} onClose={() => setError(null)} />
        {result !== null && (
          <p className="rise text-[12px] text-ok">
            Replayed {result.replayed.toLocaleString("en-US")} message{result.replayed === 1 ? "" : "s"}
            {result.skipped ? (
              <span className="text-muted">
                {" "}
                · {result.skipped.toLocaleString("en-US")} skipped (no x-death origin and no target given)
              </span>
            ) : null}
          </p>
        )}
      </form>
    </Panel>
  );
}
