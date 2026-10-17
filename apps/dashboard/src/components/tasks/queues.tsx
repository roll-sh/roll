"use client";

import { Pause, Pencil, Play, Plus, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import { formatDuration, formatRate, rel, tasks, TasksApiError, type Queue, type QueueUpdate } from "@/lib/tasks-api";
import { btn, Chip, Empty, ICON, iconBtn, InlineError, input, Loading, Panel, primary, Spinner, StateDot, useActions, useNow, usePoll } from "./ui";

export function QueuesTab() {
  const { run, busy } = useActions();
  const list = usePoll(() => tasks.queues(), 3000);
  const [editing, setEditing] = useState<string | "new" | null>(null);
  const now = useNow(10_000);
  const queues = list.data ?? [];
  const mine = queues.filter((q) => !q.name.startsWith("fn:"));
  const fns = queues.filter((q) => q.name.startsWith("fn:"));

  const act = async (key: string, fn: () => Promise<unknown>) => {
    const out = await run(key, fn);
    if (out !== undefined) list.reload();
    return out;
  };

  const remove = (q: Queue) => {
    if (!confirm(`Delete queue ${q.name}?`)) return;
    act(`del-${q.name}`, async () => {
      try {
        return await tasks.deleteQueue(q.name);
      } catch (e) {
        if (!(e instanceof TasksApiError && e.status === 409)) throw e;
        if (!confirm(`${q.name} still has unfinished messages. Cancel them and delete the queue anyway?`))
          throw new Error(`Kept ${q.name}: it still has unfinished messages.`);
        return tasks.deleteQueue(q.name, true);
      }
    });
  };

  const row = (q: Queue) => {
    const s = q.stats;
    const fn = q.name.startsWith("fn:");
    return (
      <li key={q.name} className="border-b border-line last:border-0">
        <div className="flex items-center gap-3 px-5 py-3">
          <StateDot state={q.paused ? "paused" : (s?.active ?? 0) > 0 ? "live" : (s?.ready ?? 0) > 0 ? "pending" : "delivered"} title={q.paused ? "paused" : "running"} />
          <div className="min-w-0 flex-1">
            <p className="flex items-center gap-2 truncate text-[13px]">
              <span className="truncate font-mono">{q.name}</span>
              {q.paused && <Chip>paused</Chip>}
            </p>
            <p className="mt-0.5 flex flex-wrap gap-x-2 font-mono text-[11px] text-subtle">
              <span>{q.parallelism != null ? `${q.parallelism} at once` : "unbounded"}</span>
              <span>· {q.rate ? formatRate(q.rate) : "no rate limit"}</span>
              {q.retries != null && <span>· {q.retries} retries</span>}
              {q.timeout_ms != null && <span>· {formatDuration(q.timeout_ms)} timeout</span>}
              {q.partitions > 1 && <span>· {q.partitions} partitions</span>}
              <span title={new Date(q.created_at).toLocaleString()}>· created {rel(q.created_at, now)}</span>
            </p>
          </div>
          {s && (
            <div className="hidden grid-cols-4 gap-4 text-right font-mono text-[11px] tabular-nums sm:grid">
              {(
                [
                  ["ready", s.ready],
                  ["later", s.scheduled],
                  ["active", s.active],
                  ["in flight", s.inflight],
                ] as const
              ).map(([k, v]) => (
                <div key={k} className="w-14">
                  <p className={v > 0 && (k === "active" || k === "in flight") ? "text-accent" : v > 0 ? "text-fg" : "text-subtle"}>{v}</p>
                  <p className="text-[10px] text-subtle">{k}</p>
                </div>
              ))}
            </div>
          )}
          <div className="flex items-center gap-0.5">
            <button
              title={q.paused ? "Resume" : "Pause"}
              disabled={busy !== null}
              onClick={() => act(`pause-${q.name}`, () => (q.paused ? tasks.resumeQueue(q.name) : tasks.pauseQueue(q.name)))}
              className={iconBtn}
            >
              {busy === `pause-${q.name}` ? <Spinner /> : q.paused ? <Play {...ICON} /> : <Pause {...ICON} />}
            </button>
            {!fn && (
              <>
                <button title="Edit" onClick={() => setEditing(editing === q.name ? null : q.name)} className={iconBtn}>
                  <Pencil {...ICON} />
                </button>
                <button title="Delete" disabled={busy !== null} onClick={() => remove(q)} className={`${iconBtn} hover:text-danger`}>
                  <Trash2 {...ICON} />
                </button>
              </>
            )}
          </div>
        </div>
        {editing === q.name && (
          <QueueForm
            queue={q}
            busy={busy === "queue-save"}
            onCancel={() => setEditing(null)}
            onSave={async (name, u) => {
              if ((await act("queue-save", () => tasks.putQueue(name, u))) !== undefined) setEditing(null);
            }}
          />
        )}
      </li>
    );
  };

  return (
    <div className="space-y-4">
      <Panel
        title="Queues"
        meta={list.data ? mine.length : undefined}
        actions={
          <button onClick={() => setEditing(editing === "new" ? null : "new")} className={btn}>
            <Plus {...ICON} /> New queue
          </button>
        }
      >
        {editing === "new" && (
          <QueueForm
            busy={busy === "queue-save"}
            onCancel={() => setEditing(null)}
            onSave={async (name, u) => {
              if ((await act("queue-save", () => tasks.putQueue(name, u))) !== undefined) setEditing(null);
            }}
          />
        )}
        <InlineError error={list.error} />
        {!list.data ? (
          <Loading />
        ) : mine.length === 0 ? (
          <Empty title="No queues yet">
            Queues cap parallelism and rate for the messages published to them. Publishing with{" "}
            <code className="font-mono">queue</code> creates one on the fly.
          </Empty>
        ) : (
          <ul>{mine.map(row)}</ul>
        )}
      </Panel>
      {fns.length > 0 && (
        <Panel title="Function queues" meta="managed by their app">
          <ul>{fns.map(row)}</ul>
        </Panel>
      )}
    </div>
  );
}

function QueueForm({
  queue,
  busy,
  onSave,
  onCancel,
}: {
  queue?: Queue;
  busy: boolean;
  onSave: (name: string, u: QueueUpdate) => void;
  onCancel: () => void;
}) {
  const [name, setName] = useState(queue?.name ?? "");
  const [parallelism, setParallelism] = useState(queue?.parallelism?.toString() ?? "");
  const [rate, setRate] = useState(queue?.rate ? formatRate(queue.rate) : "");
  const [retries, setRetries] = useState(queue?.retries?.toString() ?? "");
  const [timeout, setTimeout_] = useState(queue?.timeout_ms != null ? formatDuration(queue.timeout_ms) : "");
  const [partitions, setPartitions] = useState(String(queue?.partitions ?? 1));

  const submit = () => {
    const u: QueueUpdate = {
      // 0 clears parallelism; null clears the optional limits.
      parallelism: parallelism.trim() ? Number(parallelism) : queue?.parallelism != null ? 0 : undefined,
      rate: rate.trim() ? rate.trim() : queue?.rate ? null : undefined,
      retries: retries.trim() ? Number(retries) : queue?.retries != null ? null : undefined,
      timeout: timeout.trim() ? timeout.trim() : queue?.timeout_ms != null ? null : undefined,
      partitions: Number(partitions) !== (queue?.partitions ?? 1) ? Number(partitions) : undefined,
    };
    onSave(name.trim(), u);
  };

  const field = (label: string, hint: string, el: ReactNode) => (
    <label className="space-y-1">
      <span className="block text-[11px] text-subtle">{label}</span>
      {el}
      <span className="block text-[10px] text-subtle">{hint}</span>
    </label>
  );

  return (
    <form
      className="rise space-y-3 border-b border-line bg-panel-2/40 px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) submit();
      }}
    >
      <div className="grid grid-cols-2 gap-3 md:grid-cols-6">
        <div className="col-span-2">
          {field(
            "Name",
            queue ? "can't be renamed" : "letters, digits, - _ .",
            <input
              autoFocus={!queue}
              disabled={!!queue}
              value={name}
              onChange={(e) => setName(e.target.value)}
              placeholder="emails"
              className={`${input} w-full font-mono text-[12px] disabled:opacity-60`}
            />,
          )}
        </div>
        {field(
          "Parallelism",
          "empty = unbounded",
          <input value={parallelism} onChange={(e) => setParallelism(e.target.value.replace(/\D/g, ""))} placeholder="∞" className={`${input} w-full font-mono text-[12px]`} />,
        )}
        {field(
          "Rate",
          'e.g. "10/s", "100/1m"',
          <input value={rate} onChange={(e) => setRate(e.target.value)} placeholder="none" className={`${input} w-full font-mono text-[12px]`} />,
        )}
        {field(
          "Retries",
          "default for messages",
          <input value={retries} onChange={(e) => setRetries(e.target.value.replace(/\D/g, ""))} placeholder="3" className={`${input} w-full font-mono text-[12px]`} />,
        )}
        {field(
          "Timeout",
          'e.g. "30s", "5m"',
          <input value={timeout} onChange={(e) => setTimeout_(e.target.value)} placeholder="30s" className={`${input} w-full font-mono text-[12px]`} />,
        )}
      </div>
      <div className="flex flex-wrap items-end justify-between gap-3">
        <div className="w-40">
          {field(
            "Partitions",
            "only grows; raise above ~300 msg/s",
            <input value={partitions} onChange={(e) => setPartitions(e.target.value.replace(/\D/g, ""))} className={`${input} w-full font-mono text-[12px]`} />,
          )}
        </div>
        <div className="flex gap-2">
          <button type="button" onClick={onCancel} className={btn}>
            Cancel
          </button>
          <button type="submit" disabled={busy || !name.trim()} className={primary}>
            {busy ? <span className="shimmer">Saving…</span> : queue ? "Save" : "Create queue"}
          </button>
        </div>
      </div>
    </form>
  );
}
