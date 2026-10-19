"use client";

import { Ban, RotateCcw, Trash2 } from "lucide-react";
import { useState, type ReactNode } from "react";
import {
  formatDuration,
  formatTime,
  prettyBody,
  rel,
  targetSummary,
  tasks,
  type Message,
  type MessageDetail,
  type MsgState,
} from "@/lib/tasks-api";
import {
  btn,
  Chip,
  CodeBlock,
  CopyButton,
  Drawer,
  Empty,
  Field,
  ICON,
  IdLink,
  InlineError,
  input,
  LoadMore,
  Loading,
  Panel,
  Section,
  Segmented,
  StateDot,
  useActions,
  useNow,
  useOpenDetail,
  usePoll,
} from "./ui";

const STATES: { value: MsgState | "all"; label: string }[] = [
  { value: "all", label: "All" },
  { value: "pending", label: "Pending" },
  { value: "active", label: "Active" },
  { value: "delivered", label: "Delivered" },
  { value: "failed", label: "Failed" },
  { value: "canceled", label: "Canceled" },
];

const PAGE = 50;

export function MessagesTab() {
  const [state, setState] = useState<MsgState | "all">("all");
  const [queue, setQueue] = useState("");
  const [label, setLabel] = useState("");
  const [internal, setInternal] = useState(false);
  // Polling re-reads everything loaded so far, so "Load more" pages stay fresh too.
  const [limit, setLimit] = useState(PAGE);
  const [more, setMore] = useState<{ items: Message[]; cursor: string | null } | null>(null);
  const [loadingMore, setLoadingMore] = useState(false);

  const filter = { state, queue: queue || undefined, label: label.trim() || undefined, all: internal };
  const key = JSON.stringify(filter);
  const list = usePoll(() => tasks.messages({ ...filter, limit: Math.min(limit, 1000) }), 3000, key + limit);
  const queues = usePoll(() => tasks.queues(), 15_000);

  const reset = () => {
    setLimit(PAGE);
    setMore(null);
  };

  const items = more?.items ?? list.data?.items ?? [];
  const cursor = more ? more.cursor : (list.data?.cursor ?? null);

  const loadMore = async () => {
    if (!cursor) return;
    setLoadingMore(true);
    try {
      const page = await tasks.messages({ ...filter, cursor, limit: PAGE });
      setMore({ items: [...items, ...page.items], cursor: page.cursor });
      setLimit(items.length + page.items.length);
    } finally {
      setLoadingMore(false);
    }
  };

  // Once the poll catches up with the extra pages, drop the local copy.
  if (more && list.data && list.data.items.length >= more.items.length) setMore(null);

  return (
    <Panel
      title="Messages"
      meta={list.data ? `${items.length}${cursor ? "+" : ""}` : undefined}
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3">
        <Segmented
          value={state}
          options={STATES}
          onChange={(v) => {
            setState(v);
            reset();
          }}
        />
        <select
          value={queue}
          onChange={(e) => {
            setQueue(e.target.value);
            reset();
          }}
          className={`${input} text-[12px]`}
        >
          <option value="">All queues</option>
          {queues.data?.map((q) => (
            <option key={q.name} value={q.name}>
              {q.name}
            </option>
          ))}
        </select>
        <input
          value={label}
          onChange={(e) => {
            setLabel(e.target.value);
            reset();
          }}
          placeholder="label"
          className={`${input} w-32 font-mono text-[12px]`}
        />
        <label className="ml-auto inline-flex cursor-pointer items-center gap-1.5 text-[12px] text-muted">
          <input
            type="checkbox"
            checked={internal}
            onChange={(e) => {
              setInternal(e.target.checked);
              reset();
            }}
            className="accent-[var(--accent)]"
          />
          Show internal
        </label>
      </div>
      <InlineError error={list.error} />
      {!list.data ? (
        <Loading />
      ) : items.length === 0 ? (
        <Empty title={state === "all" ? "No messages yet" : `No ${state} messages`}>
          Publish with <code className="font-mono">POST /v1/tasks/publish</code> or the test form on Overview.
        </Empty>
      ) : (
        <MessageList items={items} />
      )}
      <LoadMore cursor={cursor} onMore={loadMore} loading={loadingMore} />
    </Panel>
  );
}

export function MessageList({ items, actions }: { items: Message[]; actions?: (m: Message) => ReactNode }) {
  const open = useOpenDetail();
  const now = useNow(5000);
  return (
    <ul>
      {items.map((m) => (
        <li
          key={m.id}
          onClick={() => open({ kind: "message", id: m.id })}
          className="group flex cursor-pointer items-center gap-3 border-b border-line px-5 py-2.5 transition last:border-0 hover:bg-panel-2/60"
        >
          <StateDot state={m.state} />
          <div className="min-w-0 flex-1">
            <p className="flex items-center gap-2 truncate text-[13px]">
              {m.target.type === "http" && <span className="font-mono text-[10px] text-subtle">{m.target.method}</span>}
              <span className="truncate">{targetSummary(m)}</span>
            </p>
            <p className="flex items-center gap-1.5 truncate font-mono text-[11px] text-subtle">
              <span className="truncate">{m.id}</span>
              {m.queue && <span>· {m.queue}</span>}
              {m.topic && <span>· topic {m.topic}</span>}
              {m.label && <span>· {m.label}</span>}
            </p>
          </div>
          {m.state === "failed" && m.reason && (
            <span className="hidden max-w-56 truncate font-mono text-[11px] text-danger/80 md:inline" title={m.reason}>
              {m.reason}
            </span>
          )}
          <span className="w-12 text-right font-mono text-[11px] text-subtle tabular-nums" title="attempts / allowed">
            {m.attempts}/{m.retries + 1}
          </span>
          <span className="w-24 text-right text-[12px] text-subtle" title={formatTime(m.created_at)}>
            {m.state === "pending" && m.due_at > now ? (
              <span className="text-accent">{rel(m.due_at, now)}</span>
            ) : (
              rel(m.created_at, now)
            )}
          </span>
          {actions && <span onClick={(e) => e.stopPropagation()}>{actions(m)}</span>}
        </li>
      ))}
    </ul>
  );
}

export function MessageDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const { run, busy } = useActions();
  const msg = usePoll(() => tasks.message(id), 3000, id);
  const m = msg.data;
  const final = m ? ["delivered", "failed", "canceled"].includes(m.state) : false;

  const act = async (key: string, fn: () => Promise<unknown>) => {
    const ok = await run(key, fn);
    if (ok !== undefined) msg.reload();
    return ok;
  };

  return (
    <Drawer
      onClose={onClose}
      title={
        <span className="flex items-center gap-2">
          {m && <StateDot state={m.state} />}
          <span className="font-mono text-[13px]">{id}</span>
          <CopyButton text={id} />
        </span>
      }
      actions={
        m && (
          <div className="flex items-center gap-1.5">
            {!final && (
              <button disabled={busy !== null} onClick={() => act("cancel", () => tasks.cancelMessage(id))} className={btn}>
                <Ban {...ICON} /> Cancel
              </button>
            )}
            {(m.state === "failed" || m.state === "canceled") && (m.target.type === "http" || m.target.type === "pull") && (
              <button disabled={busy !== null} onClick={() => act("retry", () => tasks.retryMessage(id))} className={btn}>
                <RotateCcw {...ICON} /> Retry
              </button>
            )}
            {final && (
              <button
                disabled={busy !== null}
                onClick={async () => {
                  if (!confirm("Purge this message and its attempt log? This can't be undone.")) return;
                  if ((await act("purge", () => tasks.purgeMessage(id))) !== undefined) onClose();
                }}
                className={`${btn} hover:text-danger`}
              >
                <Trash2 {...ICON} /> Purge
              </button>
            )}
          </div>
        )
      }
    >
      <InlineError error={msg.error} />
      {!m ? <Loading /> : <MessageBody m={m} />}
    </Drawer>
  );
}

function MessageBody({ m }: { m: MessageDetail }) {
  const now = useNow(1000);
  const t = m.target;
  const headers = t.type === "http" ? (t.headers ?? []) : [];
  const body = m.body !== undefined ? prettyBody(m.body) : null;
  return (
    <>
      <Section title="Target">
        {t.type === "http" ? (
          <div className="flex items-center gap-2 rounded-lg border border-line bg-bg px-2.5 py-2">
            <Chip>{t.method}</Chip>
            <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{t.url}</code>
            <CopyButton text={t.url} />
          </div>
        ) : (
          <p className="font-mono text-[12px]">{targetSummary(m)}</p>
        )}
        <div className="mt-3 grid grid-cols-2 gap-3 sm:grid-cols-4">
          <Field label="State">
            <span className={m.state === "failed" ? "text-danger" : m.state === "active" ? "text-accent" : ""}>{m.state}</span>
          </Field>
          <Field label="Attempts">
            {m.attempts} of {m.retries + 1}
          </Field>
          <Field label={m.state === "pending" ? "Due" : m.state === "active" ? "Lease ends" : "Updated"}>
            <span title={formatTime(m.state === "pending" || m.state === "active" ? m.due_at : m.updated_at)}>
              {rel(m.state === "pending" || m.state === "active" ? m.due_at : m.updated_at, now)}
            </span>
          </Field>
          <Field label="Created">
            <span title={formatTime(m.created_at)}>{rel(m.created_at, now)}</span>
          </Field>
          <Field label="Timeout">{formatDuration(m.timeout_ms)}</Field>
          <Field label="Backoff">
            {m.backoff.strategy} {formatDuration(m.backoff.base_ms)}–{formatDuration(m.backoff.max_ms)}
          </Field>
          {m.queue && <Field label="Queue">{m.queue}</Field>}
          {m.topic && <Field label="Topic">{m.topic}</Field>}
          {m.label && <Field label="Label">{m.label}</Field>}
          {m.group && <Field label="Group">{m.group}</Field>}
          {m.dedup_id && <Field label="Dedup id">{m.dedup_id}</Field>}
          {m.schedule_id && <Field label="Schedule">{m.schedule_id}</Field>}
          {m.run_id && (
            <Field label="Run">
              <IdLink kind="run" id={m.run_id} />
            </Field>
          )}
          {m.callback && <Field label="Callback">{m.callback}</Field>}
          {m.failure_callback && <Field label="Failure callback">{m.failure_callback}</Field>}
          {m.deferrals > 0 && <Field label="Held by flow control">{m.deferrals}×</Field>}
        </div>
        {m.reason && (
          <p className={`mt-3 rounded-lg border px-2.5 py-1.5 font-mono text-[11px] ${m.state === "failed" ? "border-danger/30 text-danger" : "border-line text-muted"}`}>
            {m.reason}
          </p>
        )}
        {m.flows && m.flows.length > 0 && (
          <div className="mt-3 flex flex-wrap gap-1.5">
            {m.flows.map((f) => (
              <Chip key={f.key} title="flow control key">
                {f.key}
                {f.parallelism != null && ` · ${f.parallelism} at once`}
                {f.rate && ` · ${f.rate.limit}/${formatDuration(f.rate.period_ms)}`}
              </Chip>
            ))}
          </div>
        )}
      </Section>

      {headers.length > 0 && (
        <Section title="Headers">
          <dl className="space-y-1 font-mono text-[11px]">
            {headers.map(([k, v], i) => (
              <div key={i} className="flex gap-2">
                <dt className="shrink-0 text-subtle">{k}:</dt>
                <dd className="min-w-0 break-all">{v}</dd>
              </div>
            ))}
          </dl>
        </Section>
      )}

      <Section
        title="Body"
        aside={
          <span className="font-mono text-[11px] text-subtle">
            {m.content_type ?? (body?.json ? "application/json" : "")} · {m.body_size} B
          </span>
        }
      >
        {m.body_base64 !== undefined ? (
          <p className="font-mono text-[11px] text-subtle">binary, {m.body_size} bytes (base64 in the API)</p>
        ) : (
          <CodeBlock raw={m.body ?? ""} />
        )}
      </Section>

      <Section title={`Attempts · ${m.attempts_log.length}`}>
        {m.attempts_log.length === 0 ? (
          <p className="text-[12px] text-subtle">
            {m.state === "pending" ? `First attempt ${rel(m.due_at, now)}.` : "No attempts recorded."}
          </p>
        ) : (
          <ol className="relative ml-1 border-l border-line">
            {m.attempts_log.map((a) => {
              const ok = a.status != null && a.status >= 200 && a.status < 300 && !a.error;
              return (
                <li key={a.n} className="relative pb-4 pl-4 last:pb-0">
                  <span
                    className={`absolute top-1.5 -left-[4.5px] size-2 rounded-full ${ok ? "bg-muted" : "bg-danger/80"}`}
                  />
                  <div className="flex flex-wrap items-baseline gap-x-2 text-[12px]">
                    <span className="font-medium">#{a.n}</span>
                    {a.status != null && <Chip tone={ok ? "default" : "danger"}>HTTP {a.status}</Chip>}
                    <span className="font-mono text-[11px] text-subtle">{formatDuration(a.duration_ms)}</span>
                    <span className="text-[11px] text-subtle" title={formatTime(a.at)}>
                      {rel(a.at, now)}
                    </span>
                    {a.next_at != null &&
                      (a.next_at > now ? (
                        <span className="text-[11px] text-accent">next retry {rel(a.next_at, now)}</span>
                      ) : (
                        <span className="text-[11px] text-subtle">retried after {formatDuration(a.next_at - a.at)}</span>
                      ))}
                  </div>
                  {a.error && <p className="mt-1 font-mono text-[11px] text-danger">{a.error}</p>}
                  {a.response && (
                    <pre className="mt-1.5 max-h-28 overflow-auto rounded-md border border-line bg-bg px-2 py-1.5 font-mono text-[10px] whitespace-pre-wrap break-all text-muted">
                      {a.response.length > 2000 ? `${a.response.slice(0, 2000)}…` : a.response}
                    </pre>
                  )}
                </li>
              );
            })}
          </ol>
        )}
      </Section>
    </>
  );
}
