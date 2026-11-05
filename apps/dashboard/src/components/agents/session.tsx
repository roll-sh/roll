"use client";

import {
  Archive,
  ArchiveRestore,
  Check,
  ChevronDown,
  ChevronRight,
  ClipboardList,
  FileDiff as DiffIcon,
  ArrowUpLeft,
  GitBranch,
  MoreHorizontal,
  Pencil,
  Plus,
  Split,
  SquareTerminal,
  Trash2,
  Upload,
  X,
} from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { coming, isBusy, machineState, reachable, type Checkpoint, type Diff, type Machine, type Request, type Run, type Terminal, type Thread } from "@/lib/agents-api";
import { checkpointsOf, nodeTree, runsOf } from "@/lib/agents-store";
import { readyPlan } from "@/lib/agents-plan";
import { DiffViewer } from "../code/diff";
import { input } from "../ui";
import { ChangeChip, PublishForm, TakeWorkForm, WorkChip } from "./change";
import { Composer } from "./composer";
import { Menu } from "./menu";
import { useInboxMenu } from "./threads-sidebar";
import { toFileDiff } from "./request-card";
import { ShareButton } from "./share";
import { Stat, StatusDot, usageLine } from "./shared";
import { MachineIcon, ProjectIcon } from "./identity";
import { TerminalView } from "./terminal";
import { Select } from "./ui";
import { orchestration, OrchestratorChip, StatusPill, threadPill } from "./thread-status";
import { EmptyTranscript, RunView, SubagentTranscript, type TranscriptActions } from "./transcript";
import { findTree, SubagentBar } from "./subagents";
import { useAgents, usePrefs } from "./use-agents";

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
  // Another conversation starts with nothing beside it, and on its own transcript.
  useEffect(() => setSide(null), [thread.id]);
  // A provider subagent's own transcript, in place of the conversation (T3 opens it as a thread).
  const [sub, setSub] = useState<string | null>(null);
  useEffect(() => setSub(null), [thread.id]);
  const subagent = useMemo(() => {
    if (!sub) return null;
    for (const r of runs) {
      const t = findTree(nodeTree(data, thread.id, r.id), sub);
      if (t) return t;
    }
    return null;
  }, [sub, runs, data, thread.id]);
  // Escape goes back to the conversation before it closes it.
  useEffect(() => {
    if (!sub) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      setSub(null);
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [sub]);
  // A delegated thread names the thread that delegated it (T3's "Open parent").
  const parent = thread.lineage?.relation === "subagent" ? data.threads.get(thread.lineage.parent)?.value : undefined;
  const terminals = [...data.terminals.values()].filter((r) => r.value.thread === thread.id).map((r) => r.value);
  const sideTerminal = side?.kind === "terminal" ? terminals.find((t) => t.id === side.id) : undefined;
  // The drawer closes when its terminal goes away, not while a just-opened one is on its way.
  const shownTerminal = useRef<string | null>(null);
  useEffect(() => {
    if (side?.kind !== "terminal") return;
    if (sideTerminal) shownTerminal.current = side.id;
    else if (shownTerminal.current === side.id) setSide(null);
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
    ? { openThread, openSubagent: setSub }
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
        openSubagent: setSub,
        hostsPlan: true,
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
  }, [thread.id, sub]);
  const onScroll = () => {
    const el = scroller.current!;
    const end = el.scrollHeight - el.scrollTop - el.clientHeight < 24;
    stick.current = end;
    setAtEnd(end);
  };

  const cpByRun = new Map(checkpoints.filter((c) => c.run).map((c) => [c.run!, c]));
  // The diff goes beside the chat only when both fit (T3 keeps 360 px for the chat).
  const body = useRef<HTMLDivElement>(null);
  const wide = useWidth(body) >= 900;

  return (
    <div role="region" aria-label={`Thread ${thread.title}`} className="flex h-full min-h-0 flex-col bg-bg text-[13px]">
      <TitleBar thread={thread} machine={machine} start={start} end={end} side={side} setSide={setSide} setError={setError} />
      <div ref={body} className="relative flex min-h-0 flex-1">
        <div className="flex min-w-0 flex-1 flex-col">
          <div className="relative min-h-0 flex-1">
            <div ref={scroller} onScroll={onScroll} className="h-full overflow-y-auto overscroll-contain select-text" data-scroll>
              <div className="mx-auto w-full max-w-[46rem] px-4 pt-4 pb-6 sm:px-6">
                {!loaded ? (
                  <EmptyTranscript>
                    <span className="shimmer">Loading the conversation…</span>
                  </EmptyTranscript>
                ) : subagent ? (
                  <SubagentTranscript t={subagent} requests={requests} actions={actions} now={now} />
                ) : runs.length === 0 ? (
                  <EmptyTranscript>{readOnly ? "No runs yet." : "Send a message to start the conversation."}</EmptyTranscript>
                ) : (
                  <div className="space-y-8">
                    {runs.map((r, i) => (
                      <RunView key={r.id} run={r} tree={nodeTree(data, thread.id, r.id)} requests={requests} checkpoint={cpByRun.get(r.id)} now={now} actions={actions} busyThread={busy} last={i === runs.length - 1} />
                    ))}
                  </div>
                )}
              </div>
            </div>
            {!atEnd && (
              // T3's "Scroll to end": a glass pill, with what's queued.
              <button
                onClick={() => {
                  stick.current = true;
                  scroller.current?.scrollTo({ top: scroller.current.scrollHeight, behavior: "smooth" });
                }}
                className="glass absolute bottom-3 left-1/2 flex h-7 -translate-x-1/2 items-center gap-1 rounded-full border border-outline px-3 text-[12px] text-muted shadow-pop transition-opacity duration-150 starting:opacity-0 hover:text-fg"
              >
                <ChevronDown className="size-3.5" /> Scroll to end
                {thread.queued.length > 0 && <span className="text-subtle">· {thread.queued.length} queued</span>}
              </button>
            )}
          </div>
          {!readOnly && (
            <div className="shrink-0 px-3 pt-1 pb-[max(0.75rem,env(safe-area-inset-bottom))] sm:px-6 sm:pb-4">
              <div className="mx-auto w-full max-w-[46rem]">
                {subagent ? (
                  <SubagentBar node={subagent.node} now={now} onParent={() => setSub(null)} />
                ) : thread.archived_at ? (
                  <p className="py-3 text-center text-[12.5px] text-subtle">Archived. Unarchive it from the menu to keep going.</p>
                ) : (
                  <>
                    {parent && <DelegatedBy parent={parent} onOpen={() => openThread(parent.id)} />}
                    <Composer key={thread.id} mode={{ kind: "thread", thread, machine }} autoFocus={!phone} />
                  </>
                )}
                {!subagent && <ContextStrip thread={thread} machine={machine} runs={runs} />}
                {error && (
                  <p className="mt-2 text-[12px] text-danger" role="alert">
                    {error}
                  </p>
                )}
              </div>
            </div>
          )}
          {sideTerminal && !readOnly && (
            <TerminalDrawer
              terminals={terminals}
              current={sideTerminal}
              machine={machine}
              onPick={(id) => setSide({ kind: "terminal", id })}
              onNew={() =>
                run(async () => {
                  const r = await client.run<{ type: "created"; id: string }>(mid!, { type: "terminal.open", thread: thread.id, cols: 100, rows: 26 }, "created");
                  setSide({ kind: "terminal", id: r.id });
                })
              }
              onClose={() => run(() => client.run(mid!, { type: "terminal.close", terminal: sideTerminal.id }))}
              onHide={() => setSide(null)}
            />
          )}
          {readOnly && error && <p className="px-6 pb-3 text-[12px] text-danger">{error}</p>}
        </div>
        {side?.kind === "diff" && !readOnly && (
          // T3's diff panel: beside the chat when there's room, over it (a sheet sliding in) when there isn't.
          <aside
            aria-label="Diff"
            className={
              wide
                ? "flex w-[min(540px,calc(100%-360px))] min-w-[360px] shrink-0 flex-col border-l border-line"
                : "absolute inset-0 z-20 flex flex-col bg-bg transition-[translate,opacity] duration-300 ease-drawer starting:translate-x-6 starting:opacity-0 motion-reduce:transition-none"
            }
          >
            <DiffPane thread={thread} machine={machine} runs={runs} run={side.run} onRun={(r) => setSide({ kind: "diff", run: r })} onClose={() => setSide(null)} />
          </aside>
        )}
      </div>
    </div>
  );
}

const DRAWER_HEIGHT = "roll:agents:terminal-height";

/** Above a delegated thread's composer: who delegated it, and the way back (T3's "Open parent"). */
function DelegatedBy({ parent, onOpen }: { parent: Thread; onOpen: () => void }) {
  return (
    <p className="flex min-w-0 items-center gap-1.5 px-3 pb-1.5 text-[12px] text-subtle">
      <span className="shrink-0">Delegated by</span>
      <span className="min-w-0 truncate text-muted">{parent.title}</span>
      <button type="button" onClick={onOpen} className="ml-auto inline-flex shrink-0 items-center gap-1 rounded-md px-1.5 py-0.5 text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg">
        <ArrowUpLeft className="size-3.5" /> Open parent
      </button>
    </p>
  );
}

/** An element's width, following it as it resizes (0 until measured). */
function useWidth(ref: React.RefObject<HTMLElement | null>): number {
  const [w, setW] = useState(0);
  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    setW(el.getBoundingClientRect().width);
    if (typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(([e]) => setW(e.contentRect.width));
    ro.observe(el);
    return () => ro.disconnect();
  }, [ref]);
  return w;
}

/**
 * The thread's terminals in a drawer under the conversation (T3's `ThreadTerminalDrawer`): as tall
 * as it was last dragged to (180 px up to three quarters of the window), its tabs and new / close /
 * hide in a slim header.
 */
function TerminalDrawer({
  terminals,
  current,
  machine,
  onPick,
  onNew,
  onClose,
  onHide,
}: {
  terminals: Terminal[];
  current: Terminal;
  machine: Machine | undefined;
  onPick: (id: string) => void;
  onNew: () => void;
  onClose: () => void;
  onHide: () => void;
}) {
  const [height, setHeight] = useState(() => {
    const saved = typeof window === "undefined" ? NaN : Number(localStorage.getItem(DRAWER_HEIGHT));
    return Number.isFinite(saved) && saved > 0 ? saved : 280;
  });
  const clamp = (h: number) => Math.round(Math.max(180, Math.min(h, window.innerHeight * 0.75)));
  const onPointerDown = (e: React.PointerEvent) => {
    e.preventDefault();
    const y0 = e.clientY;
    const h0 = height;
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    let last = h0;
    const move = (m: PointerEvent) => setHeight((last = clamp(h0 + y0 - m.clientY)));
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      try {
        localStorage.setItem(DRAWER_HEIGHT, String(last));
      } catch {
        // Not remembered.
      }
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  };
  return (
    <section
      aria-label="Terminal"
      className="relative flex shrink-0 flex-col border-t border-line bg-bg transition-[opacity,translate] duration-200 ease-drawer starting:translate-y-3 starting:opacity-0 motion-reduce:transition-none"
      style={{ height: `min(${height}px, 75%)` }}
    >
      <div role="separator" aria-orientation="horizontal" aria-label="Resize the terminal" onPointerDown={onPointerDown} className="absolute inset-x-0 -top-1 z-10 h-1.5 cursor-row-resize transition-colors hover:bg-accent/25" />
      <div className="flex h-7 shrink-0 items-center gap-0.5 px-1.5">
        <div className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto" role="tablist" aria-label="Terminals">
          {terminals.map((t) => (
            <button
              key={t.id}
              role="tab"
              aria-selected={t.id === current.id}
              onClick={() => onPick(t.id)}
              className={`flex h-6 max-w-40 shrink-0 items-center gap-1.5 rounded-md px-2 font-mono text-[11.5px] transition-colors ${t.id === current.id ? "bg-fg/[0.07] text-fg" : "text-subtle hover:text-fg"}`}
            >
              <SquareTerminal className="size-3 shrink-0" />
              <span className="truncate">{t.title}</span>
              {t.status === "exited" && <span className="text-faint">· exited</span>}
            </button>
          ))}
        </div>
        <span className="flex shrink-0 items-center text-muted">
          <IconButton label="New terminal" onClick={onNew}>
            <Plus className="size-3.5" />
          </IconButton>
          <IconButton label="Close the terminal" onClick={onClose}>
            <Trash2 className="size-3.5" />
          </IconButton>
          <IconButton label="Hide" onClick={onHide}>
            <X className="size-3.5" />
          </IconButton>
        </span>
      </div>
      <div className="min-h-0 flex-1">
        <TerminalView key={current.id} terminal={current} machine={machine} className="h-full" bare />
      </div>
    </section>
  );
}

/**
 * The plain row under the composer (T3's `ComposerSurface.ContextStrip`): where the thread runs
 * (the machine, its worktree's branch) and what it has cost and changed so far.
 */
function ContextStrip({ thread, machine, runs }: { thread: Thread; machine: Machine | undefined; runs: Run[] }) {
  const usage = usageLine(thread.usage);
  const stat = runs.findLast((r) => r.status !== "rolled_back")?.stat ?? null;
  if (!machine && !thread.worktree && !usage) return null;
  return (
    <div className="flex min-w-0 items-center gap-3 overflow-hidden px-2 pt-2 text-[11.5px] whitespace-nowrap text-subtle">
      {machine && (
        <span className="flex min-w-0 items-center gap-1" title={`On ${machine.name}`}>
          <MachineIcon machine={machine} className="size-3" />
          <span className="truncate">{machine.name}</span>
        </span>
      )}
      {thread.worktree && (
        <span className="flex min-w-0 items-center gap-1 font-mono" title={`Worktree: ${thread.worktree.path}`}>
          <GitBranch className="size-3 shrink-0" />
          <span className="truncate">{thread.worktree.branch}</span>
        </span>
      )}
      <span className="ml-auto flex shrink-0 items-center gap-2 font-mono tabular-nums">
        {usage}
        <Stat stat={stat} />
      </span>
    </div>
  );
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
  const prefs = usePrefs();
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
  const runs = runsOf(data, thread.id);
  const lastRun = runs.at(-1);
  const busy = isBusy(thread.status);
  const pending = [...data.requests.values()].map((r) => r.value).filter((r) => r.thread === thread.id && r.status === "pending");
  const pill = threadPill(thread, pending, lastRun, !!readyPlan(data, thread));
  const orchestrated = thread.orchestrator ? orchestration(thread, [...data.threads.values()].map((r) => r.value)) : null;
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
  const terminalOpen = side?.kind === "terminal";

  return (
    <div className="shrink-0 border-b border-line">
      <div className="@container/header flex h-[52px] items-center gap-1.5 px-2 sm:px-3" data-drag-region>
        {start}
        {/* T3's breadcrumb: the project, then the conversation (its switcher), then its status. */}
        {project && (
          <span className="flex max-w-40 min-w-0 shrink items-center gap-1.5 pl-1 text-[13px] text-muted @max-[26rem]/header:hidden" title={project.name}>
            <ProjectIcon project={project} className="size-3.5" />
            <span className="truncate">{project.name}</span>
            <ChevronRight className="size-3.5 shrink-0 text-faint" aria-hidden />
          </span>
        )}
        <div className="flex min-w-0 flex-1 items-center gap-2">
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
          {pill && <StatusPill pill={pill} className="@max-[22rem]/header:hidden" />}
          {sleeping && <span className={`shrink-0 truncate text-[11.5px] text-subtle @max-[30rem]/header:hidden ${coming(state) ? "shimmer" : ""}`}>{sleeping}</span>}
        </div>
        {!readOnly && (
          <div className="flex shrink-0 items-center gap-0.5 text-muted" data-nodrag>
            <IconButton label={diffOpen ? "Hide the diff" : "Diff"} onClick={() => setSide((s) => (s?.kind === "diff" ? null : { kind: "diff", run: null }))} active={diffOpen}>
              <DiffIcon className="size-4" />
            </IconButton>
            {!phone && (
              <IconButton label={terminalOpen ? "Hide the terminal" : "Terminal in this thread's directory"} disabled={!online && !terminalOpen} onClick={terminalOpen ? () => setSide(null) : openTerminal} active={terminalOpen}>
                <SquareTerminal className="size-4" />
              </IconButton>
            )}
            {!demo && !phone && <ShareButton thread={thread} />}
            <Menu
              label="More"
              trigger={<MoreHorizontal className="size-4" />}
              items={[
                { label: "Rename", icon: <Pencil />, onSelect: () => setRenaming(thread.title) },
                phone && { label: "Terminal", icon: <SquareTerminal />, onSelect: openTerminal, disabled: !online },
                { label: "Fork", icon: <Split />, onSelect: fork, disabled: !online },
                !!project?.repo && { label: "Publish as a change", icon: <Upload />, onSelect: () => setPanel("publish"), disabled: !online || busy },
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
                  // The owner's "Confirm before deleting a thread".
                  confirm: prefs.confirm_delete ? "Delete this thread?" : undefined,
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
      {(thread.change || thread.work || orchestrated) && (
        <div className="flex min-w-0 items-center gap-3 px-4 pb-2.5">
          {orchestrated && <OrchestratorChip {...orchestrated} />}
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
    <div ref={box} className="relative flex min-w-0" data-nodrag>
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
      <div className="flex h-10 items-center gap-2 border-b border-line px-3">
        <Select
          label="Diff of"
          variant="ghost"
          className="min-w-0 max-w-64"
          value={run ?? "all"}
          onChange={(v) => onRun(v === "all" ? null : v)}
          options={[
            { value: "all", label: "Whole thread" },
            ...runs.filter((r) => r.status !== "rolled_back").map((r) => ({ value: r.id, label: `Run ${r.ordinal}: ${r.prompt.slice(0, 40)}` })),
          ]}
        />
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
