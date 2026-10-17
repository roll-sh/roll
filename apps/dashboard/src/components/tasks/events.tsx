"use client";

import { Send } from "lucide-react";
import { useState } from "react";
import { formatTime, rel, tasks } from "@/lib/tasks-api";
import {
  CodeBlock,
  CopyButton,
  Drawer,
  Empty,
  Field,
  IdLink,
  InlineError,
  input,
  LoadMore,
  Loading,
  Panel,
  parseJsonInput,
  primary,
  Section,
  useActions,
  useNow,
  useOpenDetail,
  usePoll,
} from "./ui";

export function EventsTab() {
  const open = useOpenDetail();
  const [name, setName] = useState("");
  const [limit, setLimit] = useState(50);
  const list = usePoll(() => tasks.events({ name: name.trim() || undefined, limit }), 4000, `${name}|${limit}`);
  const now = useNow(5000);
  const items = list.data?.items ?? [];

  return (
    <div className="grid gap-4 lg:grid-cols-5">
      <Panel className="lg:col-span-3" title="Recent events" meta={list.data ? `${items.length}${list.data.cursor ? "+" : ""}` : undefined}>
        <div className="border-b border-line px-5 py-3">
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="Filter by exact name, e.g. order.created" className={`${input} w-full font-mono text-[12px]`} />
        </div>
        <InlineError error={list.error} />
        {!list.data ? (
          <Loading />
        ) : items.length === 0 ? (
          <Empty title="No events yet">Events start the functions that listen for them and resume runs waiting on them.</Empty>
        ) : (
          <ul>
            {items.map((e) => (
              <li
                key={e.id}
                onClick={() => open({ kind: "event", id: e.id })}
                className="flex cursor-pointer items-center gap-3 border-b border-line px-5 py-2.5 transition last:border-0 hover:bg-panel-2/60"
              >
                <div className="min-w-0 flex-1">
                  <p className="truncate font-mono text-[13px]">{e.name}</p>
                  <p className="truncate font-mono text-[11px] text-subtle">
                    {e.id} · {JSON.stringify(e.data ?? null).slice(0, 80)}
                  </p>
                </div>
                <span className={`text-[11px] ${e.runs?.length ? "text-accent" : "text-subtle"}`}>
                  {e.runs?.length ?? 0} run{e.runs?.length === 1 ? "" : "s"}
                </span>
                <span className="w-20 text-right text-[12px] text-subtle" title={formatTime(e.ts)}>
                  {rel(e.ts, now)}
                </span>
              </li>
            ))}
          </ul>
        )}
        <LoadMore cursor={list.data?.cursor} onMore={() => setLimit((l) => l + 50)} loading={false} />
      </Panel>
      <SendEvent onSent={list.reload} />
    </div>
  );
}

function SendEvent({ onSent }: { onSent: () => void }) {
  const { run, busy } = useActions();
  const open = useOpenDetail();
  const [name, setName] = useState("");
  const [data, setData] = useState('{\n  "order": 1\n}');
  const [dedup, setDedup] = useState("");

  return (
    <Panel className="self-start lg:col-span-2" title="Send an event" meta="POST /v1/tasks/events">
      <form
        className="space-y-3 px-5 py-4"
        onSubmit={async (e) => {
          e.preventDefault();
          const out = await run("send-event", async () => tasks.sendEvent(name.trim(), parseJsonInput(data) ?? {}, dedup.trim() || undefined));
          if (out?.ids[0]) {
            onSent();
            open({ kind: "event", id: out.ids[0] });
          }
        }}
      >
        <label className="block space-y-1">
          <span className="block text-[11px] text-subtle">Name</span>
          <input value={name} onChange={(e) => setName(e.target.value)} placeholder="order.created" className={`${input} w-full font-mono text-[12px]`} />
        </label>
        <label className="block space-y-1">
          <span className="block text-[11px] text-subtle">Data (JSON)</span>
          <textarea value={data} onChange={(e) => setData(e.target.value)} rows={5} spellCheck={false} className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed`} />
        </label>
        <label className="block space-y-1">
          <span className="block text-[11px] text-subtle">Id (optional — repeats within 24h are ignored)</span>
          <input value={dedup} onChange={(e) => setDedup(e.target.value)} placeholder="order-1-created" className={`${input} w-full font-mono text-[12px]`} />
        </label>
        <div className="flex justify-end">
          <button type="submit" disabled={busy !== null || !name.trim()} className={primary}>
            {busy === "send-event" ? (
              <span className="shimmer">Sending…</span>
            ) : (
              <>
                <Send className="size-3.5" strokeWidth={1.5} /> Send
              </>
            )}
          </button>
        </div>
      </form>
    </Panel>
  );
}

export function EventDrawer({ id, onClose }: { id: string; onClose: () => void }) {
  const ev = usePoll(() => tasks.event(id), 4000, id);
  const now = useNow(5000);
  const e = ev.data;
  return (
    <Drawer
      onClose={onClose}
      title={
        <span className="flex items-center gap-2">
          <span className="font-mono text-[13px]">{e?.name ?? id}</span>
          <CopyButton text={id} />
        </span>
      }
    >
      <InlineError error={ev.error} />
      {!e ? (
        <Loading />
      ) : (
        <>
          <Section title="Event">
            <div className="grid grid-cols-2 gap-3 sm:grid-cols-3">
              <Field label="Id">{e.id}</Field>
              <Field label="Received">
                <span title={formatTime(e.ts)}>{rel(e.ts, now)}</span>
              </Field>
              {e.dedup_id && <Field label="Dedup id">{e.dedup_id}</Field>}
            </div>
          </Section>
          <Section title="Data">
            <CodeBlock value={e.data ?? null} />
          </Section>
          {e.user != null && (
            <Section title="User">
              <CodeBlock value={e.user} />
            </Section>
          )}
          <Section title={`Runs started · ${e.runs?.length ?? 0}`}>
            {e.runs?.length ? (
              <ul className="space-y-1">
                {e.runs.map((r) => (
                  <li key={r}>
                    <IdLink kind="run" id={r} />
                  </li>
                ))}
              </ul>
            ) : (
              <p className="text-[12px] text-subtle">No function listens for {e.name} (or it was filtered out).</p>
            )}
          </Section>
        </>
      )}
    </Drawer>
  );
}
