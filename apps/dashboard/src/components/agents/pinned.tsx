import { ArrowDown, FileDiff as DiffIcon, MessageSquare, PanelRight, PinOff, SquareTerminal } from "lucide-react";
import { useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { isBusy, isRunning, type Checkpoint, type Machine, type Request, type Run, type Terminal, type Thread } from "@/lib/agents-api";
import { checkpointsOf, nodeTree, runsOf } from "@/lib/agents-store";
import { clampPinSize, type Pin } from "@/lib/agents-views";
import { Composer } from "./composer";
import { DiffPane } from "./session";
import { StatusDot, threadStatusLabel } from "./shared";
import { TerminalView } from "./terminal";
import { EmptyTranscript, RunView, type TranscriptActions } from "./transcript";
import { useAgents } from "./use-agents";

/** Runs a pinned conversation shows (the newest); the rest are a click away in the side panel. */
const PINNED_RUNS = 3;

/**
 * A card pinned onto a canvas view (`lib/agents-views.ts`): big, live, and laid out with the rest
 * of the canvas (it's a child of its thread, so nothing overlaps it). The header is the drag
 * handle; the corner resizes it (the size is the view's, saved when the drag ends; arrows on the
 * focused corner resize it from the keyboard).
 */
export function PinnedFrame({
  pin,
  label,
  icon,
  title,
  sub,
  tone,
  onOpen,
  onUnpin,
  onResize,
  children,
}: {
  pin: Pin;
  label: string;
  icon: ReactNode;
  title: ReactNode;
  sub?: ReactNode;
  tone?: "busy" | "error";
  onOpen?: () => void;
  onUnpin: () => void;
  onResize: (w: number, h: number) => void;
  children: ReactNode;
}) {
  const [live, setLive] = useState<{ w: number; h: number } | null>(null);
  const box = useRef<HTMLElement>(null);
  const size = live ?? { w: pin.w, h: pin.h };
  const border = tone === "busy" ? "border-accent/45" : tone === "error" ? "border-danger/50" : "border-line";

  const onPointerDown = (e: React.PointerEvent) => {
    const el = box.current;
    if (!el || e.button !== 0) return;
    e.preventDefault();
    e.stopPropagation();
    // The canvas is zoomed: a screen pixel is 1/k of the card's.
    const k = el.getBoundingClientRect().width / el.offsetWidth || 1;
    const x0 = e.clientX;
    const y0 = e.clientY;
    const from = { w: el.offsetWidth, h: el.offsetHeight };
    const handle = e.currentTarget as HTMLElement;
    handle.setPointerCapture?.(e.pointerId);
    let last = from;
    const move = (m: PointerEvent) => setLive((last = clampPinSize(pin.kind, from.w + (m.clientX - x0) / k, from.h + (m.clientY - y0) / k)));
    const up = () => {
      handle.removeEventListener("pointermove", move);
      handle.removeEventListener("pointerup", up);
      handle.removeEventListener("pointercancel", up);
      setLive(null);
      if (last.w !== pin.w || last.h !== pin.h) onResize(last.w, last.h);
    };
    handle.addEventListener("pointermove", move);
    handle.addEventListener("pointerup", up);
    handle.addEventListener("pointercancel", up);
  };
  const onKeyDown = (e: React.KeyboardEvent) => {
    const step = e.shiftKey ? 80 : 20;
    const by = ({ ArrowRight: [step, 0], ArrowLeft: [-step, 0], ArrowDown: [0, step], ArrowUp: [0, -step] } as Record<string, [number, number]>)[e.key];
    if (!by) return;
    e.preventDefault();
    e.stopPropagation();
    const next = clampPinSize(pin.kind, pin.w + by[0], pin.h + by[1]);
    onResize(next.w, next.h);
  };

  return (
    <section
      ref={box}
      aria-label={label}
      style={{ width: size.w, height: size.h }}
      className={`group/pin relative flex flex-col overflow-hidden rounded-xl border bg-panel text-[13px] shadow-[0_10px_30px_-14px_rgb(0_0_0/0.5)] transition-colors ${border}`}
    >
      <header data-drag className="flex h-10 shrink-0 cursor-grab items-center gap-2 border-b border-line px-3 active:cursor-grabbing">
        <span className="shrink-0 text-subtle [&>svg]:size-3.5">{icon}</span>
        <p className="min-w-0 flex-1 truncate text-[12.5px] font-medium tracking-[-0.01em]">{title}</p>
        {sub && <span className="max-w-[40%] shrink truncate text-[11.5px] text-subtle">{sub}</span>}
        <span className="flex shrink-0 items-center gap-0.5 text-subtle" data-nodrag>
          {onOpen && (
            <FrameButton label="Open in the side panel" onClick={onOpen}>
              <PanelRight />
            </FrameButton>
          )}
          <FrameButton label="Unpin from this view" onClick={onUnpin}>
            <PinOff />
          </FrameButton>
        </span>
      </header>
      <div className="relative flex min-h-0 flex-1 flex-col" data-nodrag>
        {children}
      </div>
      <div
        role="separator"
        aria-label="Resize"
        aria-orientation="horizontal"
        aria-valuenow={size.w}
        tabIndex={0}
        onPointerDown={onPointerDown}
        onKeyDown={onKeyDown}
        data-nodrag
        className="absolute right-0 bottom-0 z-10 grid size-4 cursor-nwse-resize place-items-center text-subtle opacity-0 transition-opacity duration-150 group-hover/pin:opacity-100 hover:text-fg focus-visible:opacity-100 [@media(hover:none)]:opacity-100"
      >
        <svg viewBox="0 0 16 16" className="size-4" aria-hidden>
          <path d="M13 6v7H6M13 10v3h-3" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" />
        </svg>
      </div>
    </section>
  );
}

function FrameButton({ label, onClick, children }: { label: string; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      onClick={(e) => {
        e.stopPropagation();
        onClick();
      }}
      title={label}
      aria-label={label}
      className="grid size-6 place-items-center rounded-md transition hover:bg-panel-2 hover:text-fg [&>svg]:size-3.5"
    >
      {children}
    </button>
  );
}

interface PinProps {
  pin: Pin;
  onUnpin: () => void;
  onResize: (w: number, h: number) => void;
}

/** A conversation on the canvas: its newest runs live, its requests answerable in place, its composer. */
export function PinnedConversation({ pin, thread, machine, onUnpin, onResize }: PinProps & { thread: Thread; machine: Machine | undefined }) {
  const { client, openThread, readOnly = false, now: clock } = useAgents();
  const data = client.data;
  const runs = runsOf(data, thread.id);
  const shown = runs.slice(-PINNED_RUNS);
  const checkpoints = checkpointsOf(data, thread.id);
  const loaded = data.detail.has(thread.id) && (data.nodes.has(thread.id) || thread.run_count === 0);
  const requests = useMemo(() => {
    const m = new Map<string, Request>();
    for (const r of data.requests.values()) if (r.value.thread === thread.id) m.set(r.value.id, r.value);
    return m;
  }, [data.requests, thread.id]);
  const busy = isBusy(thread.status);
  const [wall, setWall] = useState(() => Date.now());
  useEffect(() => {
    if (!busy || clock !== undefined) return;
    const t = setInterval(() => setWall(Date.now()), 1000);
    return () => clearInterval(t);
  }, [busy, clock]);
  const now = clock ?? wall;
  const mid = machine?.id;
  const actions: TranscriptActions = readOnly
    ? { openThread }
    : {
        respond: (request, response) => client.run(mid!, { type: "request.respond", request, response }),
        rollback: (c: Checkpoint) => client.run(mid!, { type: "checkpoint.rollback", thread: thread.id, checkpoint: c.id }),
        fork: async (r: Run) => {
          const res = await client.run<{ type: "created"; id: string }>(mid!, { type: "thread.fork", thread: thread.id, at: r.id }, "created");
          openThread(res.id);
        },
        openThread,
        savePlan: async (path, text) => (await client.run<{ type: "written"; path: string }>(mid!, { type: "fs.write", thread: thread.id, path, text }, "written")).path,
      };
  const cpByRun = new Map(checkpoints.filter((c) => c.run).map((c) => [c.run!, c]));

  // Stick to the newest work unless the reader scrolled up.
  const scroller = useRef<HTMLDivElement>(null);
  const stick = useRef(true);
  const [atEnd, setAtEnd] = useState(true);
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
  const running = isRunning(thread.status);

  return (
    <PinnedFrame
      pin={pin}
      label={`Pinned conversation ${thread.title}`}
      icon={<MessageSquare />}
      title={
        <span className="flex min-w-0 items-center gap-2">
          <StatusDot status={thread.status} />
          <span className="truncate">{thread.title}</span>
        </span>
      }
      sub={running ? <span className="shimmer">{thread.activity ?? threadStatusLabel[thread.status]}</span> : thread.status === "waiting" ? <span className="text-accent">Waiting for you</span> : undefined}
      tone={thread.status === "error" ? "error" : running ? "busy" : undefined}
      onOpen={() => openThread(thread.id)}
      onUnpin={onUnpin}
      onResize={onResize}
    >
      <div className="relative min-h-0 flex-1">
        <div ref={scroller} onScroll={onScroll} className="h-full overflow-y-auto overscroll-contain px-4 py-4 select-text" data-scroll>
          {!loaded ? (
            <EmptyTranscript>
              <span className="shimmer">Loading the conversation…</span>
            </EmptyTranscript>
          ) : runs.length === 0 ? (
            <EmptyTranscript>{readOnly ? "No runs yet." : "Nothing yet. Say what to do below."}</EmptyTranscript>
          ) : (
            <div className="space-y-7">
              {runs.length > shown.length && (
                <button onClick={() => openThread(thread.id)} className="w-full text-center text-[11.5px] text-subtle hover:text-fg">
                  {runs.length - shown.length} earlier run{runs.length - shown.length === 1 ? "" : "s"} in the side panel
                </button>
              )}
              {shown.map((r) => (
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
            className="absolute bottom-2 left-1/2 flex -translate-x-1/2 items-center gap-1 rounded-full border border-line bg-panel px-2.5 py-1 text-[11.5px] text-muted hover:text-fg"
          >
            <ArrowDown className="size-3" /> Latest
          </button>
        )}
      </div>
      {!readOnly && !thread.archived_at && (
        <div className="shrink-0 border-t border-line px-3 pt-2 pb-3">
          <Composer key={thread.id} mode={{ kind: "thread", thread, machine }} autoFocus={false} />
        </div>
      )}
    </PinnedFrame>
  );
}

/** A thread's diff on the canvas (the whole thread, or one run). */
export function PinnedDiff({ pin, thread, machine, onUnpin, onResize }: PinProps & { thread: Thread; machine: Machine | undefined }) {
  const { client, openThread } = useAgents();
  const runs = runsOf(client.data, thread.id);
  const [run, setRun] = useState<string | null>(null);
  return (
    <PinnedFrame pin={pin} label={`Pinned diff ${thread.title}`} icon={<DiffIcon />} title={thread.title} sub="Diff" onOpen={() => openThread(thread.id)} onUnpin={onUnpin} onResize={onResize}>
      <DiffPane thread={thread} machine={machine} runs={runs} run={run} onRun={setRun} onClose={onUnpin} />
    </PinnedFrame>
  );
}

/** A terminal on the canvas, as big as the view wants it. */
export function PinnedTerminal({ pin, terminal, machine, onUnpin, onResize }: PinProps & { terminal: Terminal; machine: Machine | undefined }) {
  const { client, openThread } = useAgents();
  const thread = terminal.thread ? client.data.threads.get(terminal.thread)?.value : undefined;
  return (
    <PinnedFrame
      pin={pin}
      label={`Pinned terminal ${terminal.title}`}
      icon={<SquareTerminal />}
      title={<span className="font-mono text-[12px]">{terminal.title}</span>}
      sub={terminal.status === "exited" ? "exited" : thread?.title}
      onOpen={thread ? () => openThread(thread.id) : undefined}
      onUnpin={onUnpin}
      onResize={onResize}
    >
      <TerminalView terminal={terminal} machine={machine} className="h-full" bare />
    </PinnedFrame>
  );
}
