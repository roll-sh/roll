"use client";

import { RotateCcw, Trash2 } from "lucide-react";
import { useState } from "react";
import { tasks } from "@/lib/tasks-api";
import { MessageList } from "./messages";
import { btn, Empty, ICON, iconBtn, InlineError, input, LoadMore, Loading, Panel, Spinner, useActions, usePoll } from "./ui";

export function DlqTab() {
  const { run, busy } = useActions();
  const [queue, setQueue] = useState("");
  const [limit, setLimit] = useState(50);
  const list = usePoll(() => tasks.dlq({ queue: queue || undefined, limit }), 4000, queue + limit);
  const queues = usePoll(() => tasks.queues(), 15_000);
  const items = list.data?.items ?? [];

  const act = async (key: string, fn: () => Promise<unknown>) => {
    if ((await run(key, fn)) !== undefined) list.reload();
  };

  return (
    <Panel
      title="Dead letters"
      meta={list.data ? `${items.length}${list.data.cursor ? "+" : ""}` : undefined}
      actions={
        <button
          disabled={busy !== null || items.length === 0}
          onClick={() => {
            const scope = queue ? `in ${queue}` : "";
            if (confirm(`Retry every dead-lettered message ${scope}? They're sent again now with a fresh retry budget.`))
              act("retry-all", () => tasks.retryDlq(queue ? { queue } : { all: true }));
          }}
          className={btn}
        >
          {busy === "retry-all" ? <Spinner /> : <RotateCcw {...ICON} />} Retry all
        </button>
      }
    >
      <div className="flex flex-wrap items-center gap-2 border-b border-line px-5 py-3">
        <select value={queue} onChange={(e) => setQueue(e.target.value)} className={`${input} text-[12px]`}>
          <option value="">All queues</option>
          {queues.data?.map((q) => (
            <option key={q.name} value={q.name}>
              {q.name}
            </option>
          ))}
        </select>
        <p className="text-[12px] text-subtle">
          Messages land here once every retry failed. Fix the endpoint, then retry — they keep their body and headers.
        </p>
      </div>
      <InlineError error={list.error} />
      {!list.data ? (
        <Loading />
      ) : items.length === 0 ? (
        <Empty title="The dead-letter queue is empty">Everything published was delivered or is still being retried.</Empty>
      ) : (
        <MessageList
          items={items}
          actions={(m) => (
            <span className="flex items-center gap-0.5">
              <button
                title="Retry now"
                disabled={busy !== null}
                onClick={() => act(`retry-${m.id}`, () => tasks.retryMessage(m.id))}
                className={iconBtn}
              >
                {busy === `retry-${m.id}` ? <Spinner /> : <RotateCcw {...ICON} />}
              </button>
              <button
                title="Delete"
                disabled={busy !== null}
                onClick={() => {
                  if (confirm("Delete this message from the dead-letter queue?")) act(`del-${m.id}`, () => tasks.deleteDlq(m.id));
                }}
                className={`${iconBtn} hover:text-danger`}
              >
                <Trash2 {...ICON} />
              </button>
            </span>
          )}
        />
      )}
      <LoadMore cursor={list.data?.cursor} onMore={() => setLimit((l) => l + 50)} loading={false} />
    </Panel>
  );
}
