"use client";

import { Bot, GitPullRequest, Plus, X } from "lucide-react";
import Link from "next/link";
import { useEffect, useState } from "react";
import { codeApi, type Repo, type WorkItem, type WorkState } from "@/lib/code-api";
import { ago, btn, input, panel, primary } from "../ui";
import { Field, formatLease, SourceBadge, useNow, type Run } from "./shared";

const COLUMNS: { state: WorkState; label: string; hint: string }[] = [
  { state: "open", label: "Open", hint: "waiting for an agent" },
  { state: "claimed", label: "Claimed", hint: "leased to an agent" },
  { state: "review", label: "Review", hint: "change open" },
  { state: "done", label: "Done", hint: "landed" },
];

const PRIORITIES = [
  { value: 0, label: "P0" },
  { value: 1, label: "P1" },
  { value: 2, label: "P2" },
  { value: 3, label: "P3" },
];

export function WorkTab({ repo, tick, busy, run }: { repo: Repo; tick: number; busy: string | null; run: Run }) {
  const [items, setItems] = useState<WorkItem[] | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [creating, setCreating] = useState(false);
  const [showClosed, setShowClosed] = useState(false);
  const [version, setVersion] = useState(0);
  const now = useNow();

  useEffect(() => {
    let alive = true;
    codeApi.work(repo.full_name).then(
      (w) => alive && (setItems(w), setError(null)),
      (e) => alive && setError(e instanceof Error ? e.message : String(e)),
    );
    return () => {
      alive = false;
    };
  }, [repo.full_name, tick, version]);

  const act = async (key: string, fn: () => Promise<unknown>) => {
    await run(key, fn);
    setVersion((v) => v + 1);
  };

  const closed = items?.filter((i) => i.state === "closed") ?? [];

  return (
    <div className="space-y-4">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <p className="max-w-2xl text-[13px] text-muted">
          Agents pull the highest-priority open item with{" "}
          <span className="font-mono text-[12px] text-fg">POST …/work/claim</span> and hold a lease while they work; an
          expired lease puts the item back. {repo.github && "GitHub issues sync in both directions."}
        </p>
        <button onClick={() => setCreating((c) => !c)} className={primary}>
          <Plus className="size-3.5" /> New work item
        </button>
      </div>

      {creating && (
        <NewWork
          busy={busy === "work-create"}
          onCancel={() => setCreating(false)}
          onCreate={async (body) => {
            await act("work-create", () => codeApi.createWork(repo.full_name, body));
            setCreating(false);
          }}
        />
      )}

      {error && <p className="text-[13px] text-danger">{error}</p>}

      {items === null ? (
        <p className="text-[13px] text-subtle">
          <span className="shimmer">Loading work…</span>
        </p>
      ) : (
        <div className="grid gap-3 md:grid-cols-2 xl:grid-cols-4">
          {COLUMNS.map((col) => {
            const list = items
              .filter((i) => i.state === col.state)
              .sort((a, b) => a.priority - b.priority || a.created_at - b.created_at);
            return (
              <section key={col.state} className={`${panel} flex min-h-40 flex-col`}>
                <div className="flex items-baseline justify-between border-b border-line px-4 py-3">
                  <h3 className="text-[13px] font-medium">{col.label}</h3>
                  <span className="font-mono text-[11px] text-subtle">{list.length}</span>
                </div>
                {list.length === 0 ? (
                  <p className="px-4 py-3 text-[12px] text-subtle">{col.hint}</p>
                ) : (
                  <ul className="max-h-[36rem] space-y-2 overflow-y-auto p-2">
                    {list.map((item) => (
                      <WorkCard key={item.id} repo={repo} item={item} now={now} busy={busy} act={act} />
                    ))}
                  </ul>
                )}
              </section>
            );
          })}
        </div>
      )}

      {closed.length > 0 && (
        <div>
          <button onClick={() => setShowClosed((s) => !s)} className="text-[12px] text-subtle hover:text-fg">
            {showClosed ? "Hide" : "Show"} {closed.length} closed
          </button>
          {showClosed && (
            <ul className={`${panel} rise mt-2`}>
              {closed.map((i) => (
                <li key={i.id} className="flex items-center gap-3 border-b border-line px-4 py-2 text-[13px] text-muted last:border-0">
                  <span className="min-w-0 flex-1 truncate">{i.title}</span>
                  <SourceBadge source={i.source} url={i.github_url} />
                  <span className="text-[11px] text-subtle">{ago(i.updated_at)}</span>
                </li>
              ))}
            </ul>
          )}
        </div>
      )}
    </div>
  );
}

function WorkCard({
  repo,
  item,
  now,
  busy,
  act,
}: {
  repo: Repo;
  item: WorkItem;
  now: number;
  busy: string | null;
  act: (key: string, fn: () => Promise<unknown>) => Promise<void>;
}) {
  const left = item.lease_until != null ? item.lease_until - now : null;
  return (
    <li className="rise group rounded-lg border border-line bg-bg p-3">
      <div className="flex items-start gap-2">
        <span
          className={`mt-px shrink-0 font-mono text-[10px] ${item.priority === 0 ? "text-danger" : item.priority === 1 ? "text-fg" : "text-subtle"}`}
        >
          P{item.priority}
        </span>
        <p className="min-w-0 flex-1 text-[13px] leading-snug">{item.title}</p>
      </div>
      {item.body && <p className="mt-1 line-clamp-2 text-[12px] text-subtle">{item.body}</p>}
      <div className="mt-2 flex flex-wrap items-center gap-1.5">
        <SourceBadge source={item.source} url={item.github_url} />
        {item.labels.map((l) => (
          <span key={l} className="rounded border border-line px-1.5 py-px text-[10px] text-muted">
            {l}
          </span>
        ))}
      </div>
      {item.state === "claimed" && item.claimed_by && (
        <p className="mt-2 flex items-center gap-1.5 font-mono text-[11px] text-muted">
          <Bot className="size-3.5 text-accent" />
          <span className="truncate">{item.claimed_by}</span>
          {left != null && (
            <span className={`ml-auto shrink-0 tabular-nums ${left < 30 ? "text-danger" : "text-subtle"}`} title="Lease left">
              {formatLease(left)}
            </span>
          )}
        </p>
      )}
      {item.changes.length > 0 && (
        <p className="mt-2 flex flex-wrap items-center gap-1.5 font-mono text-[11px]">
          <GitPullRequest className="size-3 text-subtle" />
          {item.changes.map((c) => (
            <Link key={c} href={`/dashboard/code/${repo.full_name}/changes/${c}`} className="text-muted hover:text-fg">
              {c}
            </Link>
          ))}
        </p>
      )}
      <div className="mt-2 flex items-center gap-1.5">
        <span className="font-mono text-[11px] text-subtle">#{item.number} · {ago(item.created_at)}</span>
        <span className="ml-auto flex gap-1 opacity-0 transition group-hover:opacity-100 focus-within:opacity-100">
          {item.state === "claimed" && (
            <button
              disabled={busy !== null}
              onClick={() => act(`release-${item.id}`, () => codeApi.release(item.id))}
              className="rounded px-1.5 py-0.5 text-[11px] text-subtle hover:bg-panel-2 hover:text-fg disabled:opacity-40"
            >
              Release
            </button>
          )}
          {item.state !== "done" && (
            <button
              disabled={busy !== null}
              onClick={() => act(`close-${item.id}`, () => codeApi.updateWork(item.id, { state: "closed" }))}
              className="rounded p-0.5 text-subtle hover:bg-panel-2 hover:text-fg disabled:opacity-40"
              title="Close"
              aria-label={`Close ${item.title}`}
            >
              <X className="size-3.5" />
            </button>
          )}
        </span>
      </div>
    </li>
  );
}

function NewWork({
  busy,
  onCreate,
  onCancel,
}: {
  busy: boolean;
  onCreate: (body: { title: string; body: string; labels: string[]; priority: number }) => void;
  onCancel: () => void;
}) {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [labels, setLabels] = useState("");
  const [priority, setPriority] = useState(2);
  return (
    <form
      className={`${panel} rise space-y-3 p-5`}
      onSubmit={(e) => {
        e.preventDefault();
        if (!title.trim()) return;
        onCreate({
          title: title.trim(),
          body,
          labels: labels
            .split(",")
            .map((l) => l.trim())
            .filter(Boolean),
          priority,
        });
      }}
    >
      <input
        autoFocus
        value={title}
        onChange={(e) => setTitle(e.target.value)}
        placeholder="What needs doing?"
        className={`${input} w-full`}
      />
      <textarea
        value={body}
        onChange={(e) => setBody(e.target.value)}
        placeholder="Context for the agent: acceptance criteria, files, links…"
        rows={4}
        className={`${input} w-full resize-y`}
      />
      <div className="grid gap-3 sm:grid-cols-[minmax(0,2fr)_minmax(0,1fr)]">
        <Field label="Labels" hint="comma-separated; agents can claim by label">
          <input
            value={labels}
            onChange={(e) => setLabels(e.target.value)}
            placeholder="bug, frontend"
            spellCheck={false}
            className={`${input} w-full`}
          />
        </Field>
        <Field label="Priority">
          <select value={priority} onChange={(e) => setPriority(Number(e.target.value))} className={`${input} w-full`}>
            {PRIORITIES.map((p) => (
              <option key={p.value} value={p.value}>
                {p.label}
              </option>
            ))}
          </select>
        </Field>
      </div>
      <div className="flex gap-2">
        <button type="submit" disabled={busy || !title.trim()} className={primary}>
          {busy ? <span className="shimmer">Creating…</span> : "Create"}
        </button>
        <button type="button" onClick={onCancel} className={btn}>
          Cancel
        </button>
      </div>
    </form>
  );
}
