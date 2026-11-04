"use client";

import {
  Archive,
  ArchiveRestore,
  ArrowDown,
  Check,
  ChevronDown,
  ClipboardList,
  FileDiff as DiffIcon,
  GitBranch,
  MoreHorizontal,
  Pencil,
  Split,
  SquareTerminal,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { coming, isBusy, machineState, reachable, type Checkpoint, type Diff, type Machine, type Request, type Run, type Thread } from "@/lib/agents-api";
import { checkpointsOf, nodeTree, providersFor, runsOf } from "@/lib/agents-store";
import { DiffViewer } from "../code/diff";
import { input } from "../ui";
import { ChangeChip, PublishForm, TakeWorkForm, WorkChip } from "./change";
import { Composer } from "./composer";
import { Menu } from "./menu";
import { useInboxMenu } from "./threads-sidebar";
import { toFileDiff } from "./request-card";
import { ShareButton } from "./share";
import { modelName, providerName, Stat, StatusDot, threadStatusLabel, usageLine } from "./shared";
import { TerminalView } from "./terminal";
import { EmptyTranscript, RunView, type TranscriptActions } from "./transcript";
import { useAgents } from "./use-agents";

/** What sits beside the transcript: a diff (of one run, or `null` for the whole thread) or a terminal. */
type Side = { kind: "diff"; run: string | null } | { kind: "terminal"; id: string } | null;

/**
 * One conversation, filling the agents surface: the transcript in a centred column, the composer
 * under it, and the diff or a terminal beside it. Everything about the thread is managed from
 * here, so there's no need to go back to the canvas. `start` is the surface's back control and
 * `end` its own buttons (new agent, close); the conversation's title bar holds both. Read-only (a
 * share's replay), it's the transcript alone.
 */
export function Conversation({ thread, machine, start, end }: { thread: Thread; machine: Machine | undefined; start?: ReactNode; end?: ReactNode }) {
  const { client, openThread, readOnly = false, now: clock, phone = false } = useAgents();
  const data = client.data;
  const runs = runsOf(data, thread.id);
  const checkpoints = checkpointsOf(data, thread.id);
  // A thread with no runs has no nodes to wait for (a fresh fork).
  const loaded = data.detail.has(thread.id) && (data.nodes.has(thread.id) || thread.run_count === 0);
  const requests = useMemo(() => {
    const m = new Map<string, Request>();
    for (const r of data.requests.values()) if (r.value.thread === thread.id) m.set(r.value.id, r.value);
    return m;
  }, [data.requests, thread.id]);
  const busy = isBusy(thread.status);
  const [wall, setNow] = useState(() => Date.now());
  useEffect(() => {
    if (!busy || clock !== undefined) return;
    const t = setInterval(() => setNow(Date.now()), 1000);
    return () => clearInterval(t);
  }, [busy, clock]);
  const now = clock ?? wall;

  const [side, setSide] = useState<Side>(null);
  // Another conversation starts with nothing beside it.
  useEffect(() => setSide(null), [thread.id]);
  const terminals = [...data.terminals.values()].filter((r) => r.value.thread === thread.id).map((r) => r.value);
  const sideTerminal = side?.kind === "terminal" ? terminals.find((t) => t.id === side.id) : undefined;
  useEffect(() => {
    if (side?.kind === "terminal" && !sideTerminal) setSide(null);
  }, [side, sideTerminal]);

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
  const actions: TranscriptActions = readOnly
    ? { openThread }
    : {
        respond: (request, response) => client.run(mid!, { type: "request.respond", request, response }),
        showDiff: (r) => setSide({ kind: "diff", run: r }),
        rollback: (c: Checkpoint) => run(() => client.run(mid!, { type: "checkpoint.rollback", thread: thread.id, checkpoint: c.id })),
        fork: (r: Run) =>
          run(async () => {
            const res = await client.run<{ type: "created"; id: string }>(mid!, { type: "thread.fork", thread: thread.id, at: r.id }, "created");
            openThread(res.id);
          }),
        openThread,
      };

  // Stick to the bottom while new work arrives, unless the reader scrolled up.
  const scroller = useRef<HTMLDivElement>(null);
  const [atEnd, setAtEnd] = useState(true);
  const stick = useRef(true);
  useLayoutEffect(() => {
    const el = scroller.current;
    if (el && stick.current) el.scrollTop = el.scrollHeight;
  });
  useEffect(() => {
    stick.current = true;
  }, [thread.id]);
  const onScroll = () => {
    const el = scroller.current!;
    const end = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    stick.current = end;
    setAtEnd(end);
  };

  const cpByRun = new Map(checkpoints.filter((c) => c.run).map((c) => [c.run!, c]));
  // On a phone whatever is beside the transcript takes its place.
  const sideOnly = phone && side !== null;

  return (
    <div role="region" aria-label={`Thread ${thread.title}`} className="flex h-full min-h-0 flex-col bg-bg text-[13px]">
      <TitleBar thread={thread} machine={machine} start={start} end={end} side={side} setSide={setSide} setError={setError} />
      <div className="flex min-h-0 flex-1">
        <div className={`relative min-w-0 flex-1 flex-col ${sideOnly ? "hidden" : "flex"}`}>
          <div ref={scroller} onScroll={onScroll} className="min-h-0 flex-1 overflow-y-auto overscroll-contain select-text" data-scroll>
            <div className="mx-auto w-full max-w-[780px] px-4 py-6 sm:px-6">
              {!loaded ? (
                <EmptyTranscript>
                  <span className="shimmer">Loading the conversation…</span>
                </EmptyTranscript>
              ) : runs.length === 0 ? (
                <EmptyTranscript>{readOnly ? "No runs yet." : "Nothing yet. Say what to do below."}</EmptyTranscript>
              ) : (
                <div className="space-y-9">
                  {runs.map((r) => (
                    <RunView key={r.id} run={r} tree={nodeTree(data, thread.id, r.id)} requests={requests} checkpoint={cpByRun.get(r.id)} now={now} actions={actions} busyThread={busy} />
                  ))}
                </div>
              )}
            </div>
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
        {side?.kind === "diff" && !readOnly && (
          <SidePanel full={phone}>
            <DiffPane thread={thread} machine={machine} runs={runs} run={side.run} onRun={(r) => setSide({ kind: "diff", run: r })} onClose={() => setSide(null)} />
          </SidePanel>
        )}
        {sideTerminal && !readOnly && (
          <SidePanel full={phone}>
            <div className="flex items-center gap-1 border-b border-line px-2 py-1.5">
              {terminals.map((t) => (
                <button
                  key={t.id}
                  onClick={() => setSide({ kind: "terminal", id: t.id })}
                  className={`flex max-w-48 items-center gap-1.5 rounded-md px-2 py-1 font-mono text-[11.5px] ${t.id === sideTerminal.id ? "bg-panel-2 text-fg" : "text-subtle hover:text-fg"}`}
                >
                  <SquareTerminal className="size-3 shrink-0" />
                  <span className="truncate">{t.title}</span>
                  {t.status === "exited" && <span className="text-subtle">· exited</span>}
                </button>
              ))}
              <span className="ml-auto flex items-center">
                <IconButton label="Close the terminal" onClick={() => run(() => client.run(mid!, { type: "terminal.close", terminal: sideTerminal.id }))}>
                  <Trash2 className="size-3.5" />
                </IconButton>
                <IconButton label="Hide" onClick={() => setSide(null)}>
                  <X className="size-3.5" />
                </IconButton>
              </span>
            </div>
            <div className="min-h-0 flex-1">
              <TerminalView key={sideTerminal.id} terminal={sideTerminal} machine={machine} className="h-full" bare />
            </div>
          </SidePanel>
        )}
      </div>
      {!readOnly && !sideOnly && (
        <div className="shrink-0 px-3 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] sm:px-6 sm:pb-5">
          <div className="mx-auto w-full max-w-[780px]">
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
              <p className="py-3 text-center text-[12.5px] text-subtle">Archived. Unarchive it from the menu to keep going.</p>
            ) : (
              <Composer key={thread.id} mode={{ kind: "thread", thread, machine }} autoFocus={!phone} />
            )}
            {error && (
              <p className="mt-2 text-[12px] text-danger" role="alert">
                {error}
              </p>
            )}
          </div>
        </div>
      )}
      {readOnly && error && <p className="px-6 pb-3 text-[12px] text-danger">{error}</p>}
    </div>
  );
}

function SidePanel({ full, children }: { full: boolean; children: ReactNode }) {
  return <div className={`flex min-h-0 shrink-0 flex-col ${full ? "w-full" : "w-[min(48%,760px)] min-w-[380px] border-l border-line"}`}>{children}</div>;
}

/** The conversation's bar: back, the title (a switcher to the other conversations), what it runs on, and every action. */
function TitleBar({
  thread,
  machine,
  start,
  end,
  side,
  setSide,
  setError,
}: {
  thread: Thread;
  machine: Machine | undefined;
  start?: ReactNode;
  end?: ReactNode;
  side: Side;
  setSide: (s: Side | ((s: Side) => Side)) => void;
  setError: (e: string | null) => void;
}) {
  const { client, openThread, close, demo, owner, services, readOnly = false, phone = false } = useAgents();
  const data = client.data;
  const [renaming, setRenaming] = useState<string | null>(null);
  const [panel, setPanel] = useState<"publish" | "work" | null>(null);
  useEffect(() => setPanel(null), [thread.id]);
  const mid = machine?.id;
  // An asleep sandbox counts: what's sent to it wakes it.
  const online = reachable(machine);
  const state = machine ? machineState(machine) : "offline";
  const sleeping = state === "asleep" ? "Asleep · wakes when an agent needs it" : state === "stopped" ? "Stopped · sending starts it" : coming(state) && machine?.sandbox ? "Waking up…" : null;
  const project = data.projects.get(thread.project)?.value;
  const providers = providersFor(data, mid);
  const model = modelName(providers, thread.provider, thread.model);
  const runs = runsOf(data, thread.id);
  const lastStat = runs.findLast((r) => r.status !== "rolled_back")?.stat ?? null;
  const busy = isBusy(thread.status);
  const usage = usageLine(thread.usage);
  // Settle, snooze (docs/code/agents.md, "Settling").
  const inboxMenu = useInboxMenu(thread);
  const act = (fn: () => Promise<unknown>) => {
    setError(null);
    fn().catch((e: Error) => setError(e.message));
  };
  const openTerminal = () =>
    act(async () => {
      const open = [...data.terminals.values()].find((r) => r.value.thread === thread.id && r.value.status !== "exited");
      if (open) return setSide({ kind: "terminal", id: open.value.id });
      const r = await client.run<{ type: "created"; id: string }>(mid!, { type: "terminal.open", thread: thread.id, cols: 100, rows: 26 }, "created");
      setSide({ kind: "terminal", id: r.id });
    });
  const fork = () =>
    act(async () => {
      const r = await client.run<{ type: "created"; id: string }>(mid!, { type: "thread.fork", thread: thread.id }, "created");
      openThread(r.id);
    });
  const diffOpen = side?.kind === "diff";

  return (
    <div className="shrink-0 border-b border-line">
      <div className="flex h-12 items-center gap-1.5 px-2 sm:px-3" data-drag-region>
        {start}
        <StatusDot status={thread.status} className="ml-1 shrink-0" />
        <div className="min-w-0 flex-1 pl-1">
          {renaming !== null ? (
            <form
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
                onKeyDown={(e) => {
                  if (e.key !== "Escape") return;
                  e.stopPropagation();
                  setRenaming(null);
                }}
                aria-label="Title"
                className={`${input} w-full max-w-md py-0.5 text-[13.5px] font-medium`}
              />
            </form>
          ) : (
            <Switcher thread={thread} onRename={readOnly ? undefined : () => setRenaming(thread.title)} />
          )}
          <p className="flex min-w-0 items-center gap-2 truncate text-[11.5px] leading-tight text-subtle">
            {busy ? <span className="shrink-0 shimmer">{thread.activity ?? threadStatusLabel[thread.status]}</span> : <span className="shrink-0">{threadStatusLabel[thread.status]}</span>}
            {sleeping && <span className={`shrink-0 ${coming(state) ? "shimmer" : ""}`}>{sleeping}</span>}
            {!phone && (
              <>
                <span className="truncate">
                  {project?.name && `${project.name} · `}
                  {providerName(providers, thread.provider)}
                  {model && ` · ${model}`}
                  {thread.effort && ` · ${thread.effort}`}
                </span>
                {thread.worktree && (
                  <span className="flex shrink-0 items-center gap-1 font-mono" title={thread.worktree.path}>
                    <GitBranch className="size-3" />
                    {thread.worktree.branch}
                  </span>
                )}
                {usage && <span className="shrink-0 font-mono">{usage}</span>}
                <Stat stat={lastStat} className="shrink-0" />
              </>
            )}
          </p>
        </div>
        {!readOnly && (
          <div className="flex shrink-0 items-center gap-0.5 text-subtle" data-nodrag>
            <IconButton label={diffOpen ? "Hide the diff" : "Diff"} onClick={() => setSide((s) => (s?.kind === "diff" ? null : { kind: "diff", run: null }))} active={diffOpen}>
              <DiffIcon className="size-4" />
            </IconButton>
            {!phone && (
              <>
                <IconButton label="Terminal in this thread's directory" disabled={!online} onClick={openTerminal} active={side?.kind === "terminal"}>
                  <SquareTerminal className="size-4" />
                </IconButton>
                <IconButton label="Fork this conversation" disabled={!online} onClick={fork}>
                  <Split className="size-4" />
                </IconButton>
                {project?.repo && (
                  <IconButton label="Publish as a roll code change" disabled={!online || busy} onClick={() => setPanel((p) => (p === "publish" ? null : "publish"))} active={panel === "publish"}>
                    <Upload className="size-4" />
                  </IconButton>
                )}
                {!demo && <ShareButton thread={thread} />}
              </>
            )}
            <Menu
              label="More"
              trigger={<MoreHorizontal className="size-4" />}
              items={[
                { label: "Rename", icon: <Pencil />, onSelect: () => setRenaming(thread.title) },
                phone && { label: "Terminal", icon: <SquareTerminal />, onSelect: openTerminal, disabled: !online },
                phone && { label: "Fork", icon: <Split />, onSelect: fork, disabled: !online },
                phone && !!project?.repo && { label: "Publish as a change", icon: <Upload />, onSelect: () => setPanel("publish"), disabled: !online || busy },
                !!services && !thread.work && { label: "Take a work item", icon: <ClipboardList />, onSelect: () => setPanel("work") },
                ...inboxMenu.items,
                {
                  label: thread.archived_at ? "Unarchive" : "Archive",
                  icon: thread.archived_at ? <ArchiveRestore /> : <Archive />,
                  disabled: !online || busy,
                  onSelect: () =>
                    act(async () => {
                      await client.run(mid!, { type: thread.archived_at ? "thread.unarchive" : "thread.archive", thread: thread.id });
                      if (!thread.archived_at) close();
                    }),
                },
                {
                  label: "Delete",
                  confirm: "Delete this thread?",
                  icon: <Trash2 />,
                  danger: true,
                  disabled: !online,
                  onSelect: () =>
                    act(async () => {
                      await client.run(mid!, { type: "thread.delete", thread: thread.id });
                      close();
                    }),
                },
              ]}
            />
          </div>
        )}
        {end && (
          <span className="flex shrink-0 items-center gap-1 pl-1" data-nodrag>
            {end}
          </span>
        )}
      </div>
      {(thread.change || thread.work) && (
        <div className="flex min-w-0 items-center gap-3 px-4 pb-2.5">
          {thread.change && <ChangeChip change={thread.change} />}
          {thread.change?.detail && <span className="min-w-0 truncate text-[11.5px] text-subtle">{thread.change.detail}</span>}
          {thread.work && <WorkChip work={thread.work} className="min-w-0 flex-1" onRelease={readOnly || !services ? undefined : () => act(() => services.releaseWork(owner, thread.id))} />}
        </div>
      )}
      {inboxMenu.extra}
      {panel === "publish" && machine && <PublishForm thread={thread} machine={machine} onDone={() => setPanel(null)} />}
      {panel === "work" && !thread.work && <TakeWorkForm thread={thread} repo={project?.repo ? `${project.repo.owner}/${project.repo.repo}` : null} onDone={() => setPanel(null)} />}
    </div>
  );
}

/** The title, and under it every other conversation, the ones that need you first. */
function Switcher({ thread, onRename }: { thread: Thread; onRename?: () => void }) {
  const { client, openThread, readOnly } = useAgents();
  const [open, setOpen] = useState(false);
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const onDown = (e: PointerEvent) => !box.current?.contains(e.target as Node) && setOpen(false);
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      setOpen(false);
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [open]);
  const data = client.data;
  const others = useMemo(() => {
    const rank = (t: Thread) => (t.pending_requests > 0 ? 0 : isBusy(t.status) ? 1 : 2);
    return [...data.threads.values()]
      .map((r) => r.value)
      .filter((t) => t.archived_at === null || t.id === thread.id)
      .sort((a, b) => rank(a) - rank(b) || b.updated_at - a.updated_at)
      .slice(0, 14);
  }, [data.threads, thread.id]);
  if (readOnly && others.length <= 1) return <h2 className="truncate text-[13.5px] leading-tight font-semibold tracking-[-0.02em]">{thread.title}</h2>;
  return (
    <div ref={box} className="relative min-w-0" data-nodrag>
      <button
        onClick={() => setOpen((o) => !o)}
        onDoubleClick={onRename}
        aria-haspopup="listbox"
        aria-expanded={open}
        title="Switch conversation (double-click to rename)"
        className="group -ml-1 flex max-w-full min-w-0 items-center gap-1 rounded-md px-1 text-left hover:bg-panel-2"
      >
        <h2 className="truncate text-[13.5px] leading-tight font-semibold tracking-[-0.02em]">{thread.title}</h2>
        <ChevronDown className="size-3.5 shrink-0 text-subtle group-hover:text-fg" />
      </button>
      {open && (
        <div role="listbox" aria-label="Conversations" className="pop absolute top-full left-0 z-50 mt-1.5 w-[min(380px,calc(100vw-1.5rem))] rounded-lg border border-line bg-panel p-1">
          {others.map((t) => {
            const p = data.projects.get(t.project)?.value;
            return (
              <button
                key={t.id}
                role="option"
                aria-selected={t.id === thread.id}
                onClick={() => {
                  setOpen(false);
                  if (t.id !== thread.id) openThread(t.id);
                }}
                className="flex w-full items-center gap-2.5 rounded-md px-2 py-1.5 text-left hover:bg-panel-2"
              >
                <StatusDot status={t.status} className="shrink-0" />
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-[12.5px]">{t.title}</span>
                  <span className="block truncate text-[11px] text-subtle">
                    {p?.name}
                    {isBusy(t.status) && t.activity ? ` · ${t.activity}` : ""}
                  </span>
                </span>
                {t.pending_requests > 0 ? (
                  <span className="rounded-full bg-accent px-1.5 font-mono text-[10.5px] font-semibold text-white tabular-nums">{t.pending_requests}</span>
                ) : t.id === thread.id ? (
                  <Check className="size-3.5 text-subtle" />
                ) : null}
              </button>
            );
          })}
        </div>
      )}
    </div>
  );
}

export function IconButton({ label, onClick, disabled, active, children }: { label: string; onClick: () => void; disabled?: boolean; active?: boolean; children: ReactNode }) {
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

/** The diff of one run (checkpoint before → after) or of the whole thread. */
export function DiffPane({
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
    <>
      <div className="flex items-center gap-2 border-b border-line px-3 py-1.5">
        <select value={run ?? ""} onChange={(e) => onRun(e.target.value || null)} aria-label="Diff of" className={`${input} min-w-0 border-transparent bg-transparent py-1 text-[12px] hover:border-line`}>
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
        <span className="ml-auto">
          <IconButton label="Close the diff" onClick={onClose}>
            <X className="size-3.5" />
          </IconButton>
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-auto overscroll-contain p-3 select-text" data-scroll>
        {error ? (
          <p className="text-[12.5px] text-danger">{error}</p>
        ) : !diff ? (
          <p className="text-[12.5px] text-subtle">
            <span className="shimmer">Loading the diff…</span>
          </p>
        ) : diff.files.length === 0 ? (
          <p className="py-10 text-center text-[12.5px] text-subtle">No changes yet.</p>
        ) : (
          <>
            {diff.truncated && <p className="mb-2 text-[12px] text-subtle">Some files were left out to keep this under 4 MB.</p>}
            <DiffViewer files={diff.files.map(toFileDiff)} />
          </>
        )}
      </div>
    </>
  );
}
