"use client";

import { DotField, HorizonMark } from "@roll/brand";
import { Check, Copy, KeyRound, Pause, Play, Plus, Send, Trash2 } from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import {
  formatBytes,
  formatUsd,
  ROLL_API,
  streams,
  type Connect,
  type KafkaInfo,
  type StreamDetail,
  type StreamInfo,
  type StreamRecord,
  type StreamUsage,
  type TopicDetail,
} from "@/lib/roll-api";
import { AppShell } from "./app-shell";
import { useMarkState } from "./mark-state";

const POLL_MS = 2500;
const TAIL_ROWS = 40;

const btn =
  "inline-flex items-center gap-1.5 rounded-lg border border-line px-2.5 py-1.5 text-[12px] text-muted transition hover:border-line-2 hover:text-fg disabled:opacity-40";
const primary =
  "inline-flex shrink-0 items-center gap-1.5 rounded-lg bg-fg px-3 py-1.5 text-[13px] font-medium text-bg transition hover:opacity-85 disabled:opacity-50";
const input =
  "rounded-lg border border-line bg-panel px-2.5 py-1.5 text-[13px] outline-none transition placeholder:text-subtle focus:border-line-2";

type Run = (key: string, fn: () => Promise<unknown>) => Promise<void>;

function useCopy() {
  const [copied, setCopied] = useState<string | null>(null);
  const copy = (key: string, text: string) => {
    navigator.clipboard?.writeText(text);
    setCopied(key);
    setTimeout(() => setCopied((c) => (c === key ? null : c)), 1200);
  };
  return { copied, copy };
}

function rate(bytesPerS: number | null) {
  return bytesPerS == null ? "—" : `${formatBytes(bytesPerS)}/s`;
}

function show(v: unknown) {
  if (v == null) return "∅";
  return typeof v === "string" ? v : JSON.stringify(v);
}

/** Bytes/s from successive `total_*_bytes` samples. */
function useThroughput(s: StreamInfo | null) {
  const prev = useRef<{ id: string; t: number; i: number; o: number } | null>(null);
  const [r, setR] = useState<{ in: number; out: number } | null>(null);
  useEffect(() => {
    if (!s) return;
    const now = performance.now();
    const cur = { id: s.id, t: now, i: s.throughput.total_in_bytes, o: s.throughput.total_out_bytes };
    const p = prev.current;
    if (p && p.id === s.id && now > p.t) {
      const dt = (now - p.t) / 1000;
      setR({ in: Math.max(0, (cur.i - p.i) / dt), out: Math.max(0, (cur.o - p.o) / dt) });
    } else if (!p || p.id !== s.id) {
      setR(null);
    }
    prev.current = cur;
  }, [s]);
  return r;
}

/** The selected stream, else the first one with topics (the auto-created default is often empty). */
function pickStream(list: StreamInfo[] | null, id: string | null) {
  return list?.find((s) => s.id === id) ?? list?.find((s) => s.topics > 0) ?? list?.[0] ?? null;
}

export function Streams() {
  const { play, setState } = useMarkState();
  const [info, setInfo] = useState<KafkaInfo | null>(null);
  const [offline, setOffline] = useState(false);
  const [list, setList] = useState<StreamInfo[] | null>(null);
  const [streamId, setStreamId] = useState<string | null>(null);
  const [detail, setDetail] = useState<StreamDetail | null>(null);
  const [topicName, setTopicName] = useState<string | null>(null);
  const [usage, setUsage] = useState<StreamUsage | null>(null);
  const [busy, setBusy] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [secret, setSecret] = useState<Connect | null>(null);

  const stream = pickStream(list, streamId);
  const sid = stream?.id;
  const throughput = useThroughput(stream);
  const topic = detail?.topic_list.find((t) => t.name === topicName) ?? detail?.topic_list[0] ?? null;

  const refresh = useCallback(async () => {
    try {
      const [i, ls] = await Promise.all([streams.info(), streams.list()]);
      setInfo(i);
      setList(ls);
      setOffline(false);
      const pick = pickStream(ls, streamId);
      setDetail(pick ? await streams.get(pick.id) : null);
    } catch {
      setOffline(true);
    }
  }, [streamId]);

  useEffect(() => {
    let alive = true;
    let timer: ReturnType<typeof setTimeout>;
    const loop = async () => {
      await refresh();
      if (alive) timer = setTimeout(loop, POLL_MS);
    };
    loop();
    return () => {
      alive = false;
      clearTimeout(timer);
    };
  }, [refresh]);

  useEffect(() => {
    if (!sid) return;
    let alive = true;
    const load = () => streams.usage(sid).then((u) => alive && setUsage(u), () => {});
    load();
    const t = setInterval(load, POLL_MS * 4);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [sid]);

  /** Run a control-plane action with the logo reflecting progress. */
  const run: Run = async (key, fn) => {
    setBusy(key);
    setError(null);
    setState("deploying");
    try {
      await fn();
      play([["success", 1400]]);
      await refresh();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
      play([["error", 1600]]);
    } finally {
      setBusy(null);
    }
  };

  return (
    <AppShell
      crumb="streams"
      actions={
        <button onClick={() => setCreating((c) => !c)} disabled={offline} className={primary}>
          <Plus className="size-3.5" /> New stream
        </button>
      }
    >
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div>
          <h1 className="text-2xl font-semibold tracking-[-0.04em]">Streams</h1>
          <p className="mt-1 text-[13px] text-muted">
            Kafka-compatible — bottomless storage in object storage, idle partitions scale to zero, nothing to size.
          </p>
        </div>
        {info && (
          <p className="font-mono text-[11px] text-subtle">
            kafka {info.bootstrap} · {info.storage}
          </p>
        )}
      </div>

      {error && (
        <p className="rise mt-4 rounded-lg border border-danger/40 bg-danger/10 px-3 py-2 text-[13px] text-danger">{error}</p>
      )}

      {creating && (
        <CreateStream
          busy={busy === "create"}
          onCancel={() => setCreating(false)}
          onCreate={(name) =>
            run("create", async () => {
              const s = await streams.create(name);
              setStreamId(s.id);
              setTopicName(null);
              setSecret(s.connect);
              setCreating(false);
            })
          }
        />
      )}

      {secret && <SecretOnce connect={secret} onDone={() => setSecret(null)} />}

      {offline ? (
        <Offline />
      ) : list === null ? (
        <p className="mt-10 text-[13px] text-subtle">
          <span className="shimmer">Connecting to rolld…</span>
        </p>
      ) : list.length === 0 || !stream ? (
        <Empty onCreate={() => setCreating(true)} />
      ) : (
        <>
          {list.length > 1 && (
            <div className="mt-6 flex flex-wrap gap-1.5">
              {list.map((s) => (
                <button
                  key={s.id}
                  onClick={() => {
                    setStreamId(s.id);
                    setTopicName(null);
                    setUsage(null);
                  }}
                  className={`rounded-full border px-3 py-1 text-[12px] transition ${
                    s.id === stream.id ? "border-line-2 bg-panel-2 text-fg" : "border-line text-muted hover:text-fg"
                  }`}
                >
                  {s.name}
                </button>
              ))}
            </div>
          )}

          <Stats stream={stream} throughput={throughput} usage={usage} />

          {detail && detail.id === stream.id && (
            <div className="mt-4 grid gap-4 lg:grid-cols-5">
              <Topics detail={detail} selected={topic?.name ?? null} onSelect={setTopicName} busy={busy} run={run} />
              <ConnectPanel detail={detail} busy={busy} run={run} onSecret={setSecret} />
            </div>
          )}

          {detail && topic && detail.id === stream.id && (
            <TopicView key={`${detail.id}/${topic.name}`} streamId={detail.id} name={topic.name} busy={busy} run={run} />
          )}

          <div className="mt-8 flex items-center justify-between border-t border-line pt-4 text-[12px] text-subtle">
            <span className="font-mono">
              {stream.id} · cluster {stream.cluster_id} · {stream.region}
            </span>
            <button
              className="inline-flex items-center gap-1.5 hover:text-danger"
              onClick={() => {
                if (confirm(`Delete ${stream.name} with all of its topics and data? This can't be undone.`)) {
                  run("delete-stream", async () => {
                    await streams.remove(stream.id);
                    setStreamId(null);
                  });
                }
              }}
            >
              <Trash2 className="size-3.5" /> Delete stream
            </button>
          </div>
        </>
      )}
    </AppShell>
  );
}

function CreateStream({ busy, onCreate, onCancel }: { busy: boolean; onCreate: (name: string) => void; onCancel: () => void }) {
  const [name, setName] = useState("");
  return (
    <form
      className="rise mt-6 flex flex-wrap items-center gap-2 rounded-2xl border border-line bg-panel p-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) onCreate(name.trim());
      }}
    >
      <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="Stream name" className={`${input} min-w-0 flex-1`} />
      <button type="submit" disabled={busy || !name.trim()} className={primary}>
        {busy ? <span className="shimmer">Creating…</span> : "Create"}
      </button>
      <button type="button" onClick={onCancel} className={btn}>
        Cancel
      </button>
    </form>
  );
}

/** A new key's secret, shown once with ready-to-paste client settings. */
function SecretOnce({ connect, onDone }: { connect: Connect; onDone: () => void }) {
  const { copied, copy } = useCopy();
  return (
    <section className="rise mt-6 rounded-2xl border border-line bg-panel">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="flex items-center gap-2 text-[15px] font-medium">
          <KeyRound className="size-4 text-accent" strokeWidth={1.5} /> New key — copy the secret now
        </h2>
        <button onClick={onDone} className={btn}>
          Done
        </button>
      </div>
      <div className="grid gap-4 px-5 py-4 lg:grid-cols-2">
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">Kafka client properties</p>
          <div className="relative rounded-lg border border-line bg-bg">
            <pre className="overflow-x-auto px-3 py-2 font-mono text-[11px] leading-relaxed">{connect.properties}</pre>
            <button onClick={() => copy("props", connect.properties)} className="absolute top-2 right-2 p-1 text-subtle hover:text-fg" title="Copy">
              {copied === "props" ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
            </button>
          </div>
        </div>
        <div className="space-y-2">
          <p className="text-[12px] text-subtle">HTTP — no Kafka client needed</p>
          <div className="relative rounded-lg border border-line bg-bg">
            <pre className="overflow-x-auto px-3 py-2 font-mono text-[11px] leading-relaxed">
              {`curl ${connect.http.produce.replace("{topic}", "events")} \\\n  -H 'authorization: ${connect.http.authorization}' \\\n  -d '{"key":"user-1","value":{"clicked":true}}'`}
            </pre>
            <button onClick={() => copy("curl", connect.http.authorization)} className="absolute top-2 right-2 p-1 text-subtle hover:text-fg" title="Copy the authorization header">
              {copied === "curl" ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
            </button>
          </div>
        </div>
      </div>
    </section>
  );
}

function Offline() {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <HorizonMark state="sleep" size={40} />
        <h2 className="text-[15px] font-medium">rolld isn&apos;t running</h2>
        <p className="max-w-md text-[13px] text-muted">
          The dashboard talks to the local control plane at <code className="font-mono text-fg">{ROLL_API}</code>. Start it
          from the repo root; Kafka clients connect on port 9092:
        </p>
        <pre className="rounded-lg border border-line bg-bg px-3 py-2 font-mono text-[12px]">cargo run -p rolld</pre>
      </div>
    </section>
  );
}

function Empty({ onCreate }: { onCreate: () => void }) {
  return (
    <section className="relative isolate mt-6 overflow-hidden rounded-2xl border border-line bg-panel">
      <DotField spacing={18} radius={90} className="-z-10 text-fg" />
      <div className="flex flex-col items-start gap-3 p-8">
        <h2 className="text-[15px] font-medium">No streams yet</h2>
        <p className="max-w-md text-[13px] text-muted">
          A stream is a Kafka cluster without brokers: any Kafka client works, or produce and consume over plain HTTP.
          Data lives in object storage, so you pay for what you keep, not for disks.
        </p>
        <button onClick={onCreate} className={primary}>
          <Plus className="size-3.5" /> New stream
        </button>
      </div>
    </section>
  );
}

function Stats({ stream, throughput, usage }: { stream: StreamInfo; throughput: { in: number; out: number } | null; usage: StreamUsage | null }) {
  const est = usage?.estimate;
  const savings = est && est.msk_monthly_usd.total > 0 ? 1 - est.monthly_usd.total / est.msk_monthly_usd.total : null;
  const stats = [
    { label: "In now", value: rate(throughput?.in ?? null), hint: `${formatBytes(stream.throughput.total_in_bytes)} since start` },
    { label: "Out now", value: rate(throughput?.out ?? null), hint: `${formatBytes(stream.throughput.total_out_bytes)} since start` },
    {
      label: "Topics",
      value: String(stream.topics),
      hint: `${stream.partitions} partition${stream.partitions === 1 ? "" : "s"}`,
    },
    {
      label: "AWS cost · projected / mo",
      value: est ? formatUsd(est.monthly_usd.total) : "—",
      hint: !est
        ? " "
        : est.monthly_usd.total > 0 && savings != null
          ? `${Math.round(savings * 100)}% below MSK (${formatUsd(est.msk_monthly_usd.total)})`
          : `vs ${formatUsd(est.msk_monthly_usd.total)} on MSK`,
      accent: true,
    },
  ];
  return (
    <div className="mt-6 grid grid-cols-2 gap-px overflow-hidden rounded-2xl border border-line bg-line lg:grid-cols-4">
      {stats.map((s) => (
        <div key={s.label} className="bg-panel p-5">
          <p className="text-[12px] text-subtle">{s.label}</p>
          <p className="mt-2 text-2xl font-semibold tracking-[-0.03em] tabular-nums">{s.value}</p>
          <p className={`mt-1 truncate text-[12px] ${s.accent ? "text-ok" : "text-muted"}`}>{s.hint}</p>
        </div>
      ))}
    </div>
  );
}

function Topics({
  detail,
  selected,
  onSelect,
  busy,
  run,
}: {
  detail: StreamDetail;
  selected: string | null;
  onSelect: (name: string) => void;
  busy: string | null;
  run: Run;
}) {
  const [name, setName] = useState("");
  const [partitions, setPartitions] = useState(3);
  const [compact, setCompact] = useState(false);
  return (
    <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-3">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Topics</h2>
        <span className="text-[12px] text-subtle">{detail.topic_list.length}</span>
      </div>
      {detail.topic_list.length === 0 ? (
        <p className="border-b border-line px-5 py-6 text-[13px] text-muted">
          No topics yet. Create one here, with any Kafka admin client, or just produce — topics are created on first use.
        </p>
      ) : (
        <ul>
          {detail.topic_list.map((t) => {
            const active = t.name === selected;
            const policy = t.configs["cleanup.policy"];
            return (
              <li
                key={t.id}
                onClick={() => onSelect(t.name)}
                className={`rise flex cursor-pointer items-center gap-3 border-b border-line px-5 py-3 transition last:border-0 ${
                  active ? "bg-panel-2" : "hover:bg-panel-2/60"
                }`}
              >
                <span className={`inline-block size-2 shrink-0 rounded-full ${active ? "bg-accent" : "border border-line-2"}`} />
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-[13px]">{t.name}</p>
                  <p className="font-mono text-[11px] text-subtle">
                    {t.partitions} partition{t.partitions === 1 ? "" : "s"}
                    {policy ? ` · ${policy}` : ""}
                  </p>
                </div>
              </li>
            );
          })}
        </ul>
      )}
      <form
        className="mt-auto flex flex-wrap items-center gap-2 border-t border-line px-5 py-3"
        onSubmit={(e) => {
          e.preventDefault();
          if (!name.trim()) return;
          run("topic", async () => {
            await streams.createTopic(detail.id, name.trim(), partitions, compact ? { "cleanup.policy": "compact" } : {});
            onSelect(name.trim());
            setName("");
          });
        }}
      >
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="new-topic" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <select value={partitions} onChange={(e) => setPartitions(Number(e.target.value))} className={`${input} text-[12px]`}>
          {[1, 3, 6, 12, 24, 48].map((n) => (
            <option key={n} value={n}>
              {n} partition{n === 1 ? "" : "s"}
            </option>
          ))}
        </select>
        <label className="flex items-center gap-1.5 text-[12px] text-muted">
          <input type="checkbox" checked={compact} onChange={(e) => setCompact(e.target.checked)} className="accent-accent" />
          compacted
        </label>
        <button type="submit" disabled={busy !== null || !name.trim()} className={btn}>
          {busy === "topic" ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
      </form>
    </section>
  );
}

function ConnectPanel({ detail, busy, run, onSecret }: { detail: StreamDetail; busy: string | null; run: Run; onSecret: (c: Connect) => void }) {
  const { copied, copy } = useCopy();
  const rows = [
    { key: "bootstrap", label: "Kafka bootstrap", value: detail.bootstrap },
    { key: "http", label: "HTTP", value: detail.http },
  ];
  return (
    <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-2">
      <div className="flex items-center justify-between border-b border-line px-5 py-4">
        <h2 className="text-[15px] font-medium">Connect</h2>
        <span className="text-[12px] text-subtle">SASL SCRAM-SHA-256 / PLAIN</span>
      </div>
      <div className="space-y-3 px-5 py-4">
        {rows.map((r) => (
          <div key={r.key} className="space-y-1">
            <p className="text-[12px] text-subtle">{r.label}</p>
            <div className="flex items-center gap-1 rounded-lg border border-line bg-bg px-2.5 py-2">
              <code className="min-w-0 flex-1 truncate font-mono text-[11px]">{r.value}</code>
              <button onClick={() => copy(r.key, r.value)} className="p-1 text-subtle hover:text-fg" title="Copy">
                {copied === r.key ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
              </button>
            </div>
          </div>
        ))}
      </div>
      <div className="border-t border-line px-5 py-4">
        <div className="flex items-center justify-between">
          <p className="text-[12px] text-subtle">API keys</p>
          <button
            disabled={busy !== null}
            onClick={() => run("key", async () => onSecret((await streams.createKey(detail.id)).connect))}
            className={btn}
          >
            <KeyRound className="size-3.5" /> New key
          </button>
        </div>
        <ul className="mt-2 space-y-1">
          {detail.keys.map((k) => (
            <li key={k.username} className="group flex items-center gap-2 font-mono text-[11px]">
              <span className="min-w-0 flex-1 truncate">{k.username}</span>
              <span className="text-subtle">{k.label}</span>
              <button
                disabled={busy !== null}
                onClick={() => {
                  if (confirm(`Revoke ${k.username}? Clients using it are disconnected on their next login.`)) {
                    run("delete-key", () => streams.deleteKey(detail.id, k.username));
                  }
                }}
                className="p-0.5 text-subtle opacity-0 transition group-hover:opacity-100 hover:text-danger"
                title="Revoke"
              >
                <Trash2 className="size-3" />
              </button>
            </li>
          ))}
        </ul>
      </div>
    </section>
  );
}

function TopicView({ streamId, name, busy, run }: { streamId: string; name: string; busy: string | null; run: Run }) {
  const { play } = useMarkState();
  const [topic, setTopic] = useState<TopicDetail | null>(null);
  const [records, setRecords] = useState<StreamRecord[]>([]);
  const [live, setLive] = useState(true);
  const [key, setKey] = useState("");
  const [value, setValue] = useState('{"hello":"roll"}');
  const [sending, setSending] = useState(false);
  const [err, setErr] = useState<string | null>(null);

  useEffect(() => {
    let alive = true;
    const load = () => streams.topic(streamId, name).then((t) => alive && setTopic(t), () => {});
    load();
    const t = setInterval(load, POLL_MS);
    return () => {
      alive = false;
      clearInterval(t);
    };
  }, [streamId, name]);

  // Live tail over SSE; the browser resumes from the last event id after a drop.
  useEffect(() => {
    if (!live) return;
    const es = new EventSource(streams.tailUrl(streamId, name));
    es.addEventListener("record", (e) => {
      const r = JSON.parse((e as MessageEvent).data) as StreamRecord;
      setRecords((rs) => [r, ...rs].slice(0, TAIL_ROWS));
    });
    return () => es.close();
  }, [streamId, name, live]);

  const send = async () => {
    if (sending || !value.trim()) return;
    setSending(true);
    setErr(null);
    try {
      await streams.produce(streamId, name, { key: key.trim() || undefined, value });
      play([["success", 900]]);
    } catch (e) {
      setErr(e instanceof Error ? e.message : String(e));
      play([["error", 1400]]);
    } finally {
      setSending(false);
    }
  };

  return (
    <div className="mt-4 grid gap-4 lg:grid-cols-5">
      <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-3">
        <div className="flex items-center justify-between border-b border-line px-5 py-4">
          <h2 className="flex items-center gap-2 text-[15px] font-medium">
            <span className="font-mono">{name}</span>
            {live && <span className="inline-block size-1.5 animate-pulse rounded-full bg-accent" title="live" />}
          </h2>
          <button onClick={() => setLive((l) => !l)} className={btn}>
            {live ? <Pause className="size-3" /> : <Play className="size-3" />} {live ? "Pause" : "Tail"}
          </button>
        </div>
        <form
          className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3"
          onSubmit={(e) => {
            e.preventDefault();
            send();
          }}
        >
          <input value={key} onChange={(e) => setKey(e.target.value)} placeholder="key" className={`${input} w-28 font-mono text-[12px]`} />
          <input value={value} onChange={(e) => setValue(e.target.value)} placeholder="value" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
          <button type="submit" disabled={sending || !value.trim()} className={btn}>
            {sending ? <HorizonMark state="thinking" size={14} interactive={false} /> : <Send className="size-3.5" />} Produce
          </button>
        </form>
        {err && <p className="rise border-b border-line px-5 py-2 font-mono text-[12px] text-danger">{err}</p>}
        <div className="max-h-80 min-h-40 overflow-auto">
          {records.length === 0 ? (
            <p className="px-5 py-6 text-[12px] text-subtle">
              {live ? <span className="shimmer">Waiting for records…</span> : "Paused."}
            </p>
          ) : (
            <table className="w-full text-left font-mono text-[11px]">
              <tbody>
                {records.map((r) => (
                  <tr key={`${r.partition}:${r.offset}`} className="rise border-b border-line last:border-0">
                    <td className="w-16 px-5 py-1.5 text-subtle tabular-nums">
                      {r.partition}:{r.offset}
                    </td>
                    <td className="max-w-32 truncate px-2 py-1.5 text-muted">{show(r.key)}</td>
                    <td className="max-w-md truncate px-2 py-1.5">{show(r.value)}</td>
                    <td className="px-5 py-1.5 text-right text-subtle">{new Date(r.timestamp).toLocaleTimeString()}</td>
                  </tr>
                ))}
              </tbody>
            </table>
          )}
        </div>
      </section>

      <section className="flex min-w-0 flex-col rounded-2xl border border-line bg-panel lg:col-span-2">
        <div className="flex items-center justify-between border-b border-line px-5 py-4">
          <h2 className="text-[15px] font-medium">Partitions</h2>
          <span className="font-mono text-[11px] text-subtle">
            {topic ? `${topic.records.toLocaleString()} records` : " "}
          </span>
        </div>
        <div className="max-h-80 overflow-auto">
          <table className="w-full text-left font-mono text-[11px]">
            <thead className="sticky top-0 bg-panel text-muted">
              <tr>
                <th className="border-b border-line px-5 py-2 font-medium">#</th>
                <th className="border-b border-line px-2 py-2 font-medium">offsets</th>
                <th className="border-b border-line px-2 py-2 text-right font-medium">local</th>
                <th className="border-b border-line px-5 py-2 text-right font-medium">object storage</th>
              </tr>
            </thead>
            <tbody>
              {topic?.partition_stats.map((p) => (
                <tr key={p.partition} className="border-b border-line last:border-0">
                  <td className="px-5 py-1.5 text-subtle">{p.partition}</td>
                  <td className="px-2 py-1.5 tabular-nums">
                    {p.log_start_offset}–{p.high_watermark}
                  </td>
                  <td className="px-2 py-1.5 text-right tabular-nums">{formatBytes(p.local_bytes)}</td>
                  <td className="px-5 py-1.5 text-right tabular-nums">
                    {formatBytes(p.remote_bytes)} <span className="text-subtle">· {p.chunks}</span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
        <div className="mt-auto flex items-center justify-between border-t border-line px-5 py-3">
          <span className="text-[11px] text-subtle">
            {topic ? `${formatBytes(topic.local_bytes)} hot on disk · ${formatBytes(topic.remote_bytes)} in object storage` : " "}
          </span>
          <button
            disabled={busy !== null}
            onClick={() => {
              if (confirm(`Delete topic ${name} and its data?`)) run("delete-topic", () => streams.deleteTopic(streamId, name));
            }}
            className="inline-flex items-center gap-1.5 text-[12px] text-subtle hover:text-danger"
          >
            <Trash2 className="size-3.5" /> Delete
          </button>
        </div>
      </section>
    </div>
  );
}
