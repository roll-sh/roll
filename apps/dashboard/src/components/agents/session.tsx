"use client";

import {
  Archive,
  ArchiveRestore,
  ArrowDown,
  FileDiff as DiffIcon,
  GitBranch,
  Minimize2,
  Pencil,
  Split,
  SquareTerminal,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import type { Checkpoint, Diff, Machine, Published, Request, Run, Thread } from "@/lib/agents-api";
import { checkpointsOf, nodeTree, providersFor, runsOf } from "@/lib/agents-store";
import { DiffViewer } from "../code/diff";
import { btn, input, primary } from "../ui";
import { Composer } from "./composer";
import { Card } from "./nodes";
import { toFileDiff } from "./request-card";
import { modelName, providerName, Stat, StatusDot, threadStatusLabel, usageLine } from "./shared";
import { EmptyTranscript, RunView, type TranscriptActions } from "./transcript";
import { useAgents } from "./use-agents";

export const SESSION_W = 880;
export const SESSION_DIFF_W = 600;
export const SESSION_H = 760;

/** A thread opened in place: transcript, composer, diff and actions, as one canvas card. */
export function ThreadSession({ thread, machine }: { thread: Thread; machine: Machine | undefined }) {
  const { client, setExpanded, focus } = useAgents();
  const data = client.data;
  const providers = providersFor(data, machine?.id);
  const runs = runsOf(data, thread.id);
  const checkpoints = checkpointsOf(data, thread.id);
  const loaded = data.detail.has(thread.id) && data.nodes.has(thread.id);
  const requests = useMemo(() => {
    const m = new Map<string, Request>();
    for (const r of data.requests.values()) if (r.value.thread === thread.id) m.set(r.value.id, r.value);
    return m;
  }, [data.requests, thread.id]);
  const busy = thread.status === "starting" || thread.status === "running" || thread.status === "waiting";
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!busy) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [busy]);

  const [diffFor, setDiffFor] = useState<string | null | undefined>(undefined);
  // The card widens or narrows with the diff: bring all of it back into view.
  const diffOpen = diffFor !== undefined;
  const firstDiff = useRef(true);
  useEffect(() => {
    if (firstDiff.current) {
      firstDiff.current = false;
      return;
    }
    focus(`t:${thread.id}`);
  }, [diffOpen, focus, thread.id]);
  const [error, setError] = useState<string | null>(null);
  const run = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      return await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const mid = machine?.id;
  const actions: TranscriptActions = {
    respond: (request, response) => client.run(mid!, { type: "request.respond", request, response }),
    showDiff: (r) => setDiffFor(r),
    rollback: (c: Checkpoint) => run(() => client.run(mid!, { type: "checkpoint.rollback", thread: thread.id, checkpoint: c.id })),
    fork: (r: Run) =>
      run(async () => {
        const res = await client.run<{ type: "created"; id: string }>(mid!, { type: "thread.fork", thread: thread.id, at: r.id }, "created");
        setExpanded(res.id, true);
      }),
    openThread: (id) => setExpanded(id, true),
  };

  // Stick to the bottom while new work arrives, unless the reader scrolled up.
  const scroller = useRef<HTMLDivElement>(null);
  const [atEnd, setAtEnd] = useState(true);
  const stick = useRef(true);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  });
  const onScroll = () => {
    const el = scroller.current!;
    const end = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    stick.current = end;
    setAtEnd(end);
  };

  const cpByRun = new Map(checkpoints.filter((c) => c.run).map((c) => [c.run!, c]));
  const project = data.projects.get(thread.project)?.value;
  const model = modelName(providers, thread.provider, thread.model);
  const lastStat = runs.length ? runs[runs.length - 1].stat : null;
  const width = diffFor !== undefined ? SESSION_W + SESSION_DIFF_W : SESSION_W;

  return (
    <Card width={width} tone={thread.status === "error" ? "error" : busy ? "busy" : undefined} label={`Thread ${thread.title}`} className="flex flex-col overflow-hidden">
      <div style={{ height: SESSION_H }} className="flex min-h-0 flex-col">
        <Header thread={thread} machine={machine} onClose={() => setExpanded(thread.id, false)} onDiff={() => setDiffFor((d) => (d === undefined ? null : undefined))} diffOpen={diffFor !== undefined} setError={setError}>
          <span className="truncate">
            {providerName(providers, thread.provider)}
            {model && ` · ${model}`}
            {thread.effort && ` · ${thread.effort}`}
          </span>
          {thread.worktree && (
            <span className="flex items-center gap-1 font-mono" title={thread.worktree.path}>
              <GitBranch className="size-3" />
              {thread.worktree.branch}
            </span>
          )}
          {usageLine(thread.usage) && <span className="font-mono">{usageLine(thread.usage)}</span>}
          <Stat stat={lastStat} />
        </Header>
        <div className="flex min-h-0 flex-1">
          <div className="relative flex min-w-0 flex-1 flex-col" data-nodrag>
            <div ref={scroller} onScroll={onScroll} className="min-h-0 flex-1 cursor-auto overflow-y-auto px-5 py-4 select-text" data-scroll>
              {!loaded ? (
                <EmptyTranscript>
                  <span className="shimmer">Loading the transcript…</span>
                </EmptyTranscript>
              ) : runs.length === 0 ? (
                <EmptyTranscript>No runs yet. Send the first message below.</EmptyTranscript>
              ) : (
                <div className="space-y-8">
                  {runs.map((r) => (
                    <RunView key={r.id} run={r} tree={nodeTree(data, thread.id, r.id)} requests={requests} checkpoint={cpByRun.get(r.id)} now={now} actions={actions} busyThread={busy} />
                  ))}
                </div>
              )}
            </div>
            {!atEnd && (
              <button
                onClick={() => {
                  stick.current = true;
                  scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
                }}
                className="absolute bottom-3 left-1/2 flex -translate-x-1/2 items-center gap-1 rounded-full border border-line bg-panel px-2.5 py-1 text-[11.5px] text-muted hover:text-fg"
              >
                <ArrowDown className="size-3" /> Latest
              </button>
            )}
          </div>
          {diffFor !== undefined && (
            <DiffPane thread={thread} machine={machine} runs={runs} run={diffFor} onRun={setDiffFor} onClose={() => setDiffFor(undefined)} />
          )}
        </div>
        <div className="border-t border-line px-4 pt-3 pb-4" data-nodrag>
          {thread.queued.length > 0 && (
            <ul className="mb-2 space-y-1" aria-label="Queued messages">
              {thread.queued.map((q) => (
                <li key={q.id} className="flex items-center gap-2 rounded-lg border border-dashed border-line-2 px-2.5 py-1.5 text-[12px]">
                  <span className="font-mono text-[10px] tracking-wide text-subtle uppercase">queued</span>
                  <span className="min-w-0 flex-1 truncate">{q.text}</span>
                  <button
                    onClick={() => run(() => client.run(mid!, { type: "message.unqueue", thread: thread.id, message: q.id }))}
                    aria-label="Remove from queue"
                    className="text-subtle hover:text-fg"
                  >
                    <X className="size-3.5" />
                  </button>
                </li>
              ))}
            </ul>
          )}
          {thread.archived_at ? (
            <p className="text-[12px] text-subtle">Archived. Unarchive it to keep going.</p>
          ) : (
            <Composer mode={{ kind: "thread", thread, machine }} autoFocus={false} />
          )}
          {error && (
            <p className="mt-2 text-[12px] text-danger" role="alert">
              {error}
            </p>
          )}
          {project && <span className="sr-only">in {project.name}</span>}
        </div>
      </div>
    </Card>
  );
}

function Header({
  thread,
  machine,
  onClose,
  onDiff,
  diffOpen,
  setError,
  children,
}: {
  thread: Thread;
  machine: Machine | undefined;
  onClose: () => void;
  onDiff: () => void;
  diffOpen: boolean;
  setError: (e: string | null) => void;
  children: React.ReactNode;
}) {
  const { client, setExpanded } = useAgents();
  const [renaming, setRenaming] = useState<string | null>(null);
  const [confirmDelete, setConfirmDelete] = useState(false);
  const [publishing, setPublishing] = useState(false);
  const mid = machine?.id;
  const project = client.data.projects.get(thread.project)?.value;
  const busy = thread.status === "starting" || thread.status === "running" || thread.status === "waiting";
  const act = (fn: () => Promise<unknown>) => {
    setError(null);
    fn().catch((e: Error) => setError(e.message));
  };

  return (
    <div className="border-b border-line" data-drag>
      <div className="flex items-center gap-2 px-4 pt-3">
        <StatusDot status={thread.status} />
        {renaming !== null ? (
          <form
            className="min-w-0 flex-1"
            onSubmit={(e) => {
              e.preventDefault();
              const title = renaming.trim();
              setRenaming(null);
              if (title && title !== thread.title) act(() => client.run(mid!, { type: "thread.configure", thread: thread.id, title }));
            }}
          >
            <input
              autoFocus
              value={renaming}
              onChange={(e) => setRenaming(e.target.value)}
              onBlur={() => setRenaming(null)}
              onKeyDown={(e) => e.key === "Escape" && setRenaming(null)}
              aria-label="Title"
              className={`${input} w-full py-0.5 text-[14px] font-medium`}
            />
          </form>
        ) : (
          <h2 className="min-w-0 flex-1 truncate text-[14.5px] font-semibold tracking-[-0.02em]" onDoubleClick={() => setRenaming(thread.title)}>
            {thread.title}
          </h2>
        )}
        <span className="shrink-0 text-[11.5px] text-subtle">{threadStatusLabel[thread.status]}</span>
        <div className="flex shrink-0 items-center gap-0.5 text-subtle">
          <IconButton label="Rename" onClick={() => setRenaming(thread.title)}>
            <Pencil className="size-3.5" />
          </IconButton>
          <IconButton label={diffOpen ? "Hide the diff" : "Diff"} onClick={onDiff} active={diffOpen}>
            <DiffIcon className="size-3.5" />
          </IconButton>
          <IconButton
            label="Terminal in this thread's directory"
            disabled={!machine?.online}
            onClick={() => act(() => client.run(mid!, { type: "terminal.open", thread: thread.id, cols: 100, rows: 26 }))}
          >
            <SquareTerminal className="size-3.5" />
          </IconButton>
          <IconButton
            label="Fork this thread"
            disabled={!machine?.online}
            onClick={() =>
              act(async () => {
                const r = await client.run<{ type: "created"; id: string }>(mid!, { type: "thread.fork", thread: thread.id }, "created");
                setExpanded(r.id, true);
              })
            }
          >
            <Split className="size-3.5" />
          </IconButton>
          {project?.repo && (
            <IconButton label="Publish as a roll code change" disabled={!machine?.online || busy} onClick={() => setPublishing((p) => !p)} active={publishing}>
              <Upload className="size-3.5" />
            </IconButton>
          )}
          <IconButton
            label={thread.archived_at ? "Unarchive" : "Archive"}
            disabled={!machine?.online || busy}
            onClick={() =>
              act(async () => {
                await client.run(mid!, { type: thread.archived_at ? "thread.unarchive" : "thread.archive", thread: thread.id });
                if (!thread.archived_at) onClose();
              })
            }
          >
            {thread.archived_at ? <ArchiveRestore className="size-3.5" /> : <Archive className="size-3.5" />}
          </IconButton>
          {confirmDelete ? (
            <span className="flex items-center gap-1 pl-1 text-[11.5px]">
              <button onClick={() => act(() => client.run(mid!, { type: "thread.delete", thread: thread.id }))} className="rounded px-1.5 py-0.5 text-danger hover:bg-danger/10">
                Delete thread
              </button>
              <button onClick={() => setConfirmDelete(false)} className="p-1 hover:text-fg" aria-label="Keep">
                <X className="size-3" />
              </button>
            </span>
          ) : (
            <IconButton label="Delete" disabled={!machine?.online} onClick={() => setConfirmDelete(true)}>
              <Trash2 className="size-3.5" />
            </IconButton>
          )}
          <IconButton label="Close (Esc)" onClick={onClose}>
            <Minimize2 className="size-3.5" />
          </IconButton>
        </div>
      </div>
      <div className="flex items-center gap-3 px-4 pt-1 pb-2.5 pl-[30px] text-[11.5px] text-subtle">{children}</div>
      {publishing && machine && <Publish thread={thread} machine={machine} onDone={() => setPublishing(false)} />}
    </div>
  );
}

function IconButton({ label, onClick, disabled, active, children }: { label: string; onClick: () => void; disabled?: boolean; active?: boolean; children: React.ReactNode }) {
  return (
    <button
      onClick={onClick}
      disabled={disabled}
      title={label}
      aria-label={label}
      aria-pressed={active}
      className={`grid size-7 place-items-center rounded-md transition hover:bg-panel-2 hover:text-fg disabled:opacity-30 ${active ? "bg-panel-2 text-fg" : ""}`}
    >
      {children}
    </button>
  );
}

function Publish({ thread, machine, onDone }: { thread: Thread; machine: Machine; onDone: () => void }) {
  const { client } = useAgents();
  const [message, setMessage] = useState(thread.title);
  const [land, setLand] = useState(false);
  const [busy, setBusy] = useState(false);
  const [done, setDone] = useState<Published | null>(null);
  const [error, setError] = useState<string | null>(null);
  if (done)
    return (
      <p className="flex items-center gap-2 border-t border-line px-4 py-2.5 text-[12.5px]" data-nodrag>
        Published <span className="font-mono text-muted">{done.branch}</span>
        {done.url && (
          <a href={done.url} className="text-accent hover:underline">
            change {done.change}
          </a>
        )}
        <button onClick={onDone} className="ml-auto text-subtle hover:text-fg" aria-label="Close">
          <X className="size-3.5" />
        </button>
      </p>
    );
  return (
    <form
      data-nodrag
      className="flex items-center gap-2 border-t border-line px-4 py-2.5"
      onSubmit={async (e) => {
        e.preventDefault();
        setBusy(true);
        setError(null);
        try {
          setDone(await client.run<{ type: "published" } & Published>(machine.id, { type: "thread.publish", thread: thread.id, message, land }, "published"));
        } catch (err) {
          setError(err instanceof Error ? err.message : String(err));
        } finally {
          setBusy(false);
        }
      }}
    >
      <input value={message} onChange={(e) => setMessage(e.target.value)} aria-label="Commit message" className={`${input} min-w-0 flex-1`} />
      <label className="flex items-center gap-1.5 text-[12px] text-muted">
        <input type="checkbox" checked={land} onChange={(e) => setLand(e.target.checked)} className="accent-[var(--accent)]" /> and land
      </label>
      <button type="submit" disabled={busy || !message.trim()} className={primary}>
        {busy ? <span className="shimmer">Publishing…</span> : "Publish"}
      </button>
      {error && <span className="text-[12px] text-danger">{error}</span>}
    </form>
  );
}

/** The diff of one run (checkpoint before → after) or of the whole thread. */
function DiffPane({
  thread,
  machine,
  runs,
  run,
  onRun,
  onClose,
}: {
  thread: Thread;
  machine: Machine | undefined;
  runs: Run[];
  run: string | null;
  onRun: (r: string | null) => void;
  onClose: () => void;
}) {
  const { client } = useAgents();
  const [diff, setDiff] = useState<Diff | null>(null);
  const [error, setError] = useState<string | null>(null);
  const lastRun = runs[runs.length - 1];
  // Re-fetch when the scope changes or a run finishes.
  const version = `${run}:${lastRun?.id}:${lastRun?.status}`;
  useEffect(() => {
    if (!machine) return;
    let live = true;
    setDiff(null);
    setError(null);
    client
      .run<{ type: "diff" } & Diff>(machine.id, { type: "diff.get", thread: thread.id, ...(run ? { run } : {}) }, "diff")
      .then((d) => live && setDiff(d))
      .catch((e: Error) => live && setError(e.message));
    return () => {
      live = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [client, machine?.id, thread.id, version]);

  return (
    <div className="flex w-[600px] shrink-0 flex-col border-l border-line" data-nodrag>
      <div className="flex items-center gap-2 border-b border-line px-3 py-2">
        <select value={run ?? ""} onChange={(e) => onRun(e.target.value || null)} aria-label="Diff of" className={`${input} py-1 text-[12px]`}>
          <option value="">Whole thread</option>
          {runs
            .filter((r) => r.status !== "rolled_back")
            .map((r) => (
              <option key={r.id} value={r.id}>
                Run {r.ordinal}: {r.prompt.slice(0, 40)}
              </option>
            ))}
        </select>
        {diff && <Stat stat={diff.stat} className="text-[11.5px]" />}
        <button onClick={onClose} className={`${btn} ml-auto`} aria-label="Close the diff">
          <X className="size-3.5" />
        </button>
      </div>
      <div className="min-h-0 flex-1 cursor-auto overflow-auto p-3 select-text" data-scroll>
        {error ? (
          <p className="text-[12.5px] text-danger">{error}</p>
        ) : !diff ? (
          <p className="text-[12.5px] text-subtle">
            <span className="shimmer">Loading the diff…</span>
          </p>
        ) : (
          <>
            {diff.truncated && <p className="mb-2 text-[12px] text-subtle">Some files were left out to keep this under 4 MB.</p>}
            <DiffViewer files={diff.files.map(toFileDiff)} />
          </>
        )}
      </div>
    </div>
  );
}
