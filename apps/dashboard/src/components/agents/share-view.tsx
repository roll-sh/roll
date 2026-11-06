"use client";

/**
 * A shared thread at `/agents/s/<id>`: the conversation, read-only, with a replay (and the canvas
 * of it and its subagents a click away). The page renders the
 * final frame (what the server sent and what a crawler sees), then rewinds and plays the run
 * back from its records' times (`lib/agents-replay.ts`): threads and nodes appear, text streams,
 * busy edges pulse, approvals wait and get answered. A scrubber, play/pause, steps and speed
 * control it. With reduced motion it stays on the final frame, and playing steps through the
 * states without streaming text.
 */

import { HorizonMark, RollLogo, useReducedMotion } from "@roll/brand";
import { ChevronLeft, ChevronRight, Folder, MessageSquare, Network, Pause, Play, RotateCcw, Server } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { Share } from "@/lib/agents-api";
import { AgentsClient, type AgentsTransport } from "@/lib/agents-client";
import { deriveGraph, type GraphNode } from "@/lib/agents-graph";
import type { Size } from "@/lib/agents-layout";
import { frameAt, realAt, replayAt, shareStats, step, timeline } from "@/lib/agents-replay";
import { useMarkState } from "../mark-state";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { Card, RequestNode, SubagentNode, ThreadNode } from "./nodes";
import { Conversation, IconButton } from "./session";
import { Select } from "./ui";
import { AgentsContext, useMarkDriver, type AgentsCtx } from "./use-agents";

export const SPEEDS = [0.5, 1, 2, 4, 8] as const;

/** A share's canvas has nothing to talk to. */
const offline: AgentsTransport = {
  state: async () => ({ cursor: 0, machines: [], entities: [] }),
  stream: () => () => {},
  command: async () => ({ type: "error", code: "invalid", message: "a shared thread is read-only" }),
};

/** m:ss of the replay clock. */
export function clock(ms: number) {
  const s = Math.max(0, Math.round(ms / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, "0")}`;
}

const OS: Record<string, string> = { macos: "macOS", linux: "Linux", windows: "Windows" };

export function ShareView({ share }: { share: Share }) {
  const reduced = useReducedMotion();
  const tl = useMemo(() => timeline(share), [share]);
  const stats = useMemo(() => shareStats(share), [share]);
  // The server renders the final frame; the replay starts once the page is running here.
  const [ms, setMs] = useState(tl.duration);
  const [playing, setPlaying] = useState(false);
  const [speed, setSpeed] = useState<(typeof SPEEDS)[number]>(1);
  // The shared conversation first; the canvas shows it with its forks and subagents.
  const [open, setOpen] = useState<string | null>(share.thread);
  const canvas = useRef<CanvasHandle>(null);
  const started = useRef(false);

  // Rewind and play once, unless motion is reduced. Started only once it plays: an effect run
  // that's cleaned up first (strict mode's) must not leave it rewound at 0:00 for good.
  useEffect(() => {
    if (started.current || reduced || tl.duration === 0) return;
    setMs(0);
    const t = setTimeout(() => {
      started.current = true;
      setPlaying(true);
    }, 500);
    return () => clearTimeout(t);
  }, [reduced, tl.duration]);

  // The replay clock: continuous with motion, one state at a time without.
  useEffect(() => {
    if (!playing) return;
    if (reduced) {
      const t = setInterval(() => setMs((m) => (m >= tl.duration ? m : step(tl, m, 1))), 700 / speed);
      return () => clearInterval(t);
    }
    let raf = 0;
    let last = performance.now();
    const tick = (now: number) => {
      const dt = Math.min(100, now - last);
      last = now;
      setMs((m) => Math.min(tl.duration, m + dt * speed));
      raf = requestAnimationFrame(tick);
    };
    raf = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(raf);
  }, [playing, reduced, speed, tl]);
  useEffect(() => {
    if (playing && ms >= tl.duration) setPlaying(false);
  }, [playing, ms, tl.duration]);

  // A frame every 40 ms of replay is smooth enough and keeps the work per frame small.
  const shown = playing && !reduced ? Math.floor(ms / 40) * 40 : ms;
  const real = realAt(tl, shown);
  const data = useMemo(() => frameAt(share, tl, real, { stream: !reduced }), [share, tl, real, reduced]);
  const client = useMemo(() => new AgentsClient(`share:${share.id}`, offline), [share.id]);
  // Nothing subscribes to this client: the frame is handed to it for the cards that read `client.data`.
  client.show(data);
  useMarkDriver(useMemo(() => ({ data, conn: "live" as const, error: null }), [data]));

  const openThread = useCallback((thread: string) => setOpen(thread), []);
  const close = useCallback(() => setOpen(null), []);
  const focus = useCallback((key: string) => {
    if (key.startsWith("t:")) return setOpen(key.slice(2));
    setOpen(null);
    setTimeout(() => canvas.current?.focus(key), 60);
  }, []);
  const ctx: AgentsCtx = useMemo(
    () => ({ client, owner: share.owner, demo: false, openThread, close, focus, readOnly: true, now: real }),
    [client, share.owner, openThread, close, focus, real],
  );

  const graph = useMemo(() => deriveGraph(data), [data]);
  const thread = open ? data.threads.get(open) : undefined;
  const render = (n: GraphNode) => {
    switch (n.kind) {
      case "machine":
        return <SharedMachineCard name={share.machine.name} os={share.machine.os} />;
      case "project": {
        const p = data.projects.get(n.id)!.value;
        return <SharedProjectCard name={p.name} path={p.path} />;
      }
      case "thread": {
        const t = data.threads.get(n.id)!.value;
        const m = data.machines.get(n.machine)?.value;
        return <ThreadNode thread={t} machine={m} />;
      }
      case "subagent": {
        const node = [...data.nodes.values()].map((x) => x.get(n.id)).find(Boolean)?.value;
        return node ? <SubagentNode node={node} /> : null;
      }
      case "request":
        return <RequestNode request={data.requests.get(n.id)!.value} />;
      case "terminal":
      case "pin":
        return null;
    }
  };
  const defaultSize = useCallback(
    (n: GraphNode): Size => {
      switch (n.kind) {
        case "machine":
          return { w: 240, h: 72 };
        case "project":
          return { w: 260, h: 72 };
        case "thread":
          return { w: 296, h: 96 };
        case "subagent":
          return { w: 248, h: 84 };
        case "request":
          return { w: 340, h: 160 };
        case "terminal":
        case "pin":
          return { w: 0, h: 0 };
      }
    },
    [],
  );

  const busy = [...data.threads.values()].some((r) => ["starting", "running", "waiting"].includes(r.value.status));
  const atEnd = ms >= tl.duration;
  const toggle = () => {
    if (atEnd) setMs(0);
    setPlaying((p) => !p || atEnd);
  };

  return (
    <AgentsContext.Provider value={ctx}>
      <div className="flex h-dvh flex-col bg-bg text-fg">
        <ShareHeader share={share} stats={stats} />
        <div className="relative min-h-0 flex-1">
          {thread ? (
            <Conversation
              thread={thread.value}
              machine={data.machines.get(thread.machine)?.value}
              end={
                graph.nodes.size > 3 ? (
                  <IconButton label="The canvas: this thread, its forks and subagents" onClick={close}>
                    <Network className="size-4" />
                  </IconButton>
                ) : undefined
              }
            />
          ) : (
            <AgentsCanvas
              ref={canvas}
              graph={graph}
              storageKey={`share:${share.id}`}
              render={render}
              defaultSize={defaultSize}
              onEscape={() => setOpen(share.thread)}
              overlay={
                <button onClick={() => setOpen(share.thread)} className="absolute top-3 left-3 inline-flex items-center gap-1.5 rounded-lg border border-line bg-panel/90 px-2.5 py-1.5 text-[12px] text-muted backdrop-blur hover:text-fg">
                  <MessageSquare className="size-3.5" /> Back to the conversation
                </button>
              }
            />
          )}
        </div>
        <div className="flex flex-wrap items-center gap-3 border-t border-line bg-panel px-4 py-2.5" role="group" aria-label="Replay">
          <div className="flex items-center gap-1">
            <CtlButton label="Back to the start" onClick={() => setMs(0)}>
              <RotateCcw className="size-3.5" />
            </CtlButton>
            <CtlButton label="Previous step" onClick={() => setMs((m) => step(tl, m, -1))}>
              <ChevronLeft className="size-4" />
            </CtlButton>
            <button
              onClick={toggle}
              aria-label={playing ? "Pause" : atEnd ? "Replay" : "Play"}
              title={playing ? "Pause" : atEnd ? "Replay from the start" : "Play"}
              className="grid size-8 place-items-center rounded-full bg-fg text-bg transition hover:opacity-85"
            >
              {playing ? <Pause className="size-3.5" /> : <Play className="size-3.5 translate-x-px" />}
            </button>
            <CtlButton label="Next step" onClick={() => setMs((m) => step(tl, m, 1))}>
              <ChevronRight className="size-4" />
            </CtlButton>
          </div>
          <span className="flex items-center gap-1.5 font-mono text-[11.5px] text-subtle tabular-nums">
            <span className={`size-1.5 rounded-full ${busy && !atEnd ? "bg-accent" : "bg-subtle/60"}`} aria-hidden />
            {clock(ms)} / {clock(tl.duration)}
          </span>
          <Scrubber share={share} value={ms} max={tl.duration} tl={tl} onChange={(v) => setMs(v)} onGrab={() => setPlaying(false)} />
          <span className="flex items-center gap-1 text-[11.5px] text-subtle">
            Speed
            <Select
              label="Speed"
              variant="ghost"
              className="font-mono"
              value={String(speed)}
              onChange={(v) => setSpeed(Number(v) as (typeof SPEEDS)[number])}
              options={SPEEDS.map((s) => ({ value: String(s), label: `${s}×` }))}
            />
          </span>
          <span className="ml-auto hidden text-[11px] text-subtle sm:inline">
            {new Date(real).toLocaleString(undefined, { dateStyle: "medium", timeStyle: "medium" })}
          </span>
        </div>
      </div>
    </AgentsContext.Provider>
  );
}

function ShareHeader({ share, stats }: { share: Share; stats: ReturnType<typeof shareStats> }) {
  const { state } = useMarkState();
  return (
    <header className="flex items-center gap-4 border-b border-line px-4 py-2.5">
      <Link href="/" aria-label="roll" className="shrink-0">
        <RollLogo state={state} size={17} />
      </Link>
      <span className="h-4 w-px bg-line" aria-hidden />
      <div className="min-w-0 flex-1">
        <h1 className="truncate text-[14px] font-semibold tracking-[-0.02em]">{share.title}</h1>
        <p className="truncate text-[11.5px] text-subtle">
          Shared by <span className="text-muted">{share.owner}</span> · {share.machine.name}
          {share.machine.os ? ` (${OS[share.machine.os] ?? share.machine.os})` : ""} · {stats.runs} run{stats.runs === 1 ? "" : "s"} · {stats.steps} step
          {stats.steps === 1 ? "" : "s"}
          {stats.threads > 1 ? ` · ${stats.threads} threads` : ""} · {new Date(share.created_at).toLocaleDateString(undefined, { dateStyle: "medium" })}
          {share.trimmed ? " · long outputs shortened" : ""}
        </p>
      </div>
      <Link href="/dashboard/agents" aria-label="Open roll agents" className="flex shrink-0 items-center gap-1.5 text-[12px] text-subtle hover:text-fg">
        <HorizonMark size={13} interactive={false} label="" /> <span className="max-sm:hidden">keep the code rolling</span>
      </Link>
    </header>
  );
}

/** The timeline: the replay clock, with a tick where each run starts. */
function Scrubber({ share, value, max, tl, onChange, onGrab }: { share: Share; value: number; max: number; tl: ReturnType<typeof timeline>; onChange: (v: number) => void; onGrab: () => void }) {
  const ticks = useMemo(
    () =>
      share.entities.flatMap((e) => (e.kind === "run" && max > 0 ? [{ id: e.id, at: replayAt(tl, e.requested_at) / max }] : [])),
    [share, tl, max],
  );
  return (
    <div className="relative min-w-40 flex-1">
      <input
        type="range"
        min={0}
        max={Math.max(1, max)}
        step={10}
        value={Math.min(value, max)}
        onPointerDown={onGrab}
        onChange={(e) => onChange(Number(e.target.value))}
        aria-label="Replay position"
        className="relative z-10 w-full accent-[var(--accent)]"
      />
      <div className="pointer-events-none absolute inset-x-0 -bottom-1 h-1" aria-hidden>
        {ticks.map((t) => (
          <span key={t.id} className="absolute h-1 w-px bg-line-2" style={{ left: `${t.at * 100}%` }} />
        ))}
      </div>
    </div>
  );
}

function CtlButton({ label, onClick, children }: { label: string; onClick: () => void; children: React.ReactNode }) {
  return (
    <button onClick={onClick} title={label} aria-label={label} className="grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg">
      {children}
    </button>
  );
}

function SharedMachineCard({ name, os }: { name: string; os: string | null }) {
  return (
    <Card width={240} label={`Machine ${name}`}>
      <div className="flex items-center gap-2 px-3.5 py-3">
        <Server className="size-4 text-muted" />
        <span className="min-w-0 flex-1 truncate font-medium tracking-[-0.01em]">{name}</span>
        {os && <span className="font-mono text-[11px] text-subtle">{OS[os] ?? os}</span>}
      </div>
    </Card>
  );
}

function SharedProjectCard({ name, path }: { name: string; path: string }) {
  return (
    <Card width={260} label={`Project ${name}`}>
      <div className="px-3.5 py-3">
        <p className="flex items-center gap-2 font-medium tracking-[-0.01em]">
          <Folder className="size-4 text-muted" /> {name}
        </p>
        <p className="mt-1 truncate pl-6 font-mono text-[11px] text-subtle" title={path}>
          {path}
        </p>
      </div>
    </Card>
  );
}
