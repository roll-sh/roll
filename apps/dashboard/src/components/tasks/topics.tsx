"use client";

import { Plus, Radio, Trash2, X } from "lucide-react";
import { useState } from "react";
import { rel, tasks, type Topic, type TopicEndpoint } from "@/lib/tasks-api";
import { btn, CopyButton, Empty, ICON, iconBtn, InlineError, input, Loading, Panel, primary, useActions, useNow, usePoll } from "./ui";

/** "https://a.com/x billing" → {url, name}; one endpoint per line. */
function parseEndpoints(text: string): TopicEndpoint[] {
  return text
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean)
    .map((l) => {
      const [url, ...name] = l.split(/\s+/);
      return name.length ? { url, name: name.join(" ") } : { url };
    });
}

export function TopicsTab() {
  const { run, busy } = useActions();
  const list = usePoll(() => tasks.topics(), 5000);
  const [creating, setCreating] = useState(false);
  const topics = list.data ?? [];

  const act = async (key: string, fn: () => Promise<unknown>) => {
    const out = await run(key, fn);
    if (out !== undefined) list.reload();
    return out;
  };

  return (
    <div className="space-y-4">
      <Panel
        title="Topics"
        meta={list.data ? topics.length : undefined}
        actions={
          <button onClick={() => setCreating((c) => !c)} className={btn}>
            <Plus {...ICON} /> New topic
          </button>
        }
      >
        {creating && (
          <NewTopic
            busy={busy === "topic-new"}
            onCancel={() => setCreating(false)}
            onCreate={async (name, eps) => {
              if ((await act("topic-new", () => tasks.putTopic(name, eps))) !== undefined) setCreating(false);
            }}
          />
        )}
        <InlineError error={list.error} />
        {!list.data ? (
          <Loading />
        ) : topics.length === 0 ? (
          <Empty title="No topics yet">
            A topic fans one publish out to every endpoint; each copy is retried on its own.
          </Empty>
        ) : (
          <ul>
            {topics.map((t) => (
              <TopicRow key={t.name} topic={t} act={act} />
            ))}
          </ul>
        )}
      </Panel>
    </div>
  );
}

function TopicRow({ topic: t, act }: { topic: Topic; act: (key: string, fn: () => Promise<unknown>) => Promise<unknown> }) {
  const { busy } = useActions();
  const now = useNow(10_000);
  const [url, setUrl] = useState("");
  const [name, setName] = useState("");
  return (
    <li className="border-b border-line last:border-0">
      <div className="flex items-center gap-3 px-5 pt-3.5 pb-2">
        <Radio className="size-4 text-subtle" strokeWidth={1.5} />
        <div className="min-w-0 flex-1">
          <p className="font-mono text-[13px]">{t.name}</p>
          <p className="text-[11px] text-subtle">
            {t.endpoints.length} endpoint{t.endpoints.length === 1 ? "" : "s"} · updated {rel(t.updated_at, now)}
          </p>
        </div>
        <button
          title="Delete topic"
          disabled={busy !== null}
          onClick={() => {
            if (confirm(`Delete topic ${t.name}? Messages already fanned out keep going.`)) act(`del-${t.name}`, () => tasks.deleteTopic(t.name));
          }}
          className={`${iconBtn} hover:text-danger`}
        >
          <Trash2 {...ICON} />
        </button>
      </div>
      <ul className="ml-12 mr-5 border-l border-line">
        {t.endpoints.map((e) => (
          <li key={e.url} className="group flex items-center gap-2 py-1 pl-3">
            <code className="min-w-0 flex-1 truncate font-mono text-[12px]">{e.url}</code>
            {e.name && <span className="text-[11px] text-subtle">{e.name}</span>}
            <span className="opacity-0 transition group-hover:opacity-100">
              <CopyButton text={e.url} />
            </span>
            <button
              title="Remove endpoint"
              disabled={busy !== null}
              onClick={() => act(`rm-${t.name}`, () => tasks.removeEndpoints(t.name, [e.url]))}
              className={`${iconBtn} hover:text-danger`}
            >
              <X {...ICON} />
            </button>
          </li>
        ))}
      </ul>
      <form
        className="ml-12 mr-5 flex flex-wrap items-center gap-2 py-3"
        onSubmit={async (e) => {
          e.preventDefault();
          if (!url.trim()) return;
          const ep = name.trim() ? { url: url.trim(), name: name.trim() } : url.trim();
          if ((await act(`add-${t.name}`, () => tasks.addEndpoints(t.name, [ep]))) !== undefined) {
            setUrl("");
            setName("");
          }
        }}
      >
        <input value={url} onChange={(e) => setUrl(e.target.value)} placeholder="https://… add endpoint" className={`${input} min-w-0 flex-1 font-mono text-[12px]`} />
        <input value={name} onChange={(e) => setName(e.target.value)} placeholder="name (optional)" className={`${input} w-36 text-[12px]`} />
        <button type="submit" disabled={busy !== null || !url.trim()} className={btn}>
          Add
        </button>
      </form>
    </li>
  );
}

function NewTopic({ busy, onCreate, onCancel }: { busy: boolean; onCreate: (name: string, eps: TopicEndpoint[]) => void; onCancel: () => void }) {
  const [name, setName] = useState("");
  const [eps, setEps] = useState("");
  return (
    <form
      className="rise space-y-3 border-b border-line bg-panel-2/40 px-5 py-4"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) onCreate(name.trim(), parseEndpoints(eps));
      }}
    >
      <label className="block space-y-1">
        <span className="block text-[11px] text-subtle">Name</span>
        <input autoFocus value={name} onChange={(e) => setName(e.target.value)} placeholder="orders" className={`${input} w-full max-w-xs font-mono text-[12px]`} />
      </label>
      <label className="block space-y-1">
        <span className="block text-[11px] text-subtle">Endpoints — one URL per line, optionally followed by a name</span>
        <textarea
          value={eps}
          onChange={(e) => setEps(e.target.value)}
          rows={3}
          spellCheck={false}
          placeholder={"https://billing.example.com/hooks/orders billing\nhttps://mail.example.com/hooks/orders"}
          className={`${input} block w-full resize-y font-mono text-[12px] leading-relaxed`}
        />
      </label>
      <div className="flex justify-end gap-2">
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
        <button type="submit" disabled={busy || !name.trim()} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create topic"}
        </button>
      </div>
    </form>
  );
}
