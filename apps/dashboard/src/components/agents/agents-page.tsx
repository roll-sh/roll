"use client";

import { DotField, RollLogo, RollMark } from "@roll/brand";
import { Archive, ArrowLeft, MoreHorizontal, Plus, Server, Share2, X } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { isBusy } from "@/lib/agents-api";
import { setActiveClient, type AgentsClient } from "@/lib/agents-client";
import { deriveGraph, rolling, type GraphNode } from "@/lib/agents-graph";
import type { Size } from "@/lib/agents-layout";
import type { AgentsServices } from "@/lib/agents-services";
import { setAgentsView, useAgentsSurface, type AgentsView } from "@/lib/agents-surface";
import { desktop, onDesktopDeepLink, useDesktop } from "@/lib/desktop";
import { useMarkState } from "../mark-state";
import { AddMachineDialog, AddMachinePaths } from "./add-machine";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { Menu } from "./menu";
import { NewAgent } from "./new-agent";
import { MachineNode, ProjectNode, RequestNode, SubagentNode, ThreadNode } from "./nodes";
import { RequestCard } from "./request-card";
import { useSandboxes } from "./sandbox";
import { Conversation, IconButton } from "./session";
import { TerminalNode } from "./terminal";
import { AgentsContext, useAgentsSnapshot, useHold, useMarkDriver, type AgentsCtx } from "./use-agents";
import { useAgentsConnection } from "./use-connection";

/** Threads whose detail the canvas follows besides the open one, so their subagents show. */
const MAX_FOLLOWED_BUSY = 12;

const PHONE = "(max-width: 640px)";

/** A phone-sized screen. */
export function usePhone(): boolean {
  return useSyncExternalStore(
    (fn) => {
      const mq = window.matchMedia(PHONE);
      mq.addEventListener("change", fn);
      return () => mq.removeEventListener("change", fn);
    },
    () => window.matchMedia(PHONE).matches,
    () => false,
  );
}

/**
 * `/dashboard/agents`: the agents surface filling the window. The desktop app's Agents window
 * shows this page; in a browser the header's popup is the usual way in.
 */
export function AgentsPage({ demo = false, owner, focus, fresh = false }: { demo?: boolean; owner?: string; focus?: string; fresh?: boolean }) {
  const { client, services, error } = useAgentsConnection({ owner, demo });
  // Where the window opens (`?new`, `?focus=t:thr_…`), before the first paint.
  useLayoutEffect(() => {
    if (fresh) setAgentsView({ kind: "new" });
    else if (focus?.startsWith("t:")) setAgentsView({ kind: "thread", thread: focus.slice(2) });
    // Only where it opens; later views are the user's.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  const mac = typeof window !== "undefined" && desktop()?.window === "agents" && desktop()?.platform === "darwin";
  return (
    <div className="h-dvh bg-bg text-fg" data-desktop={mac ? "agents-mac" : undefined}>
      {client && services ? (
        <AgentsSurface client={client} services={services} demo={demo} chrome="window" focus={focus && !focus.startsWith("t:") ? focus : undefined} />
      ) : (
        <CenterNote shimmer={!error}>{error ?? "Connecting…"}</CenterNote>
      )}
    </div>
  );
}

export function CenterNote({ children, shimmer = false }: { children: ReactNode; shimmer?: boolean }) {
  return (
    <div className="grid h-full place-items-center px-6 text-center text-[13px] text-subtle">
      <span className={shimmer ? "shimmer" : ""}>{children}</span>
    </div>
  );
}

/**
 * The agents: the canvas of everything, one conversation, or a new agent's composer, as the store
 * says (`lib/agents-surface.ts`). The popup and the full window both show this; `onClose` is the
 * popup's (Escape on the canvas calls it).
 */
export function AgentsSurface({
  client,
  services,
  demo,
  chrome,
  onClose,
  focus: initialFocus,
}: {
  client: AgentsClient;
  services: AgentsServices;
  demo: boolean;
  chrome: "popup" | "window";
  onClose?: () => void;
  /** A canvas node to show first (`m:…`, `x:…`). */
  focus?: string;
}) {
  useHold(client);
  useEffect(() => {
    setActiveClient(client);
    return () => setActiveClient(null);
  }, [client]);
  const snap = useAgentsSnapshot(client)!;
  const data = snap.data;
  const phone = usePhone();
  const sandboxes = useSandboxes(services, client.owner, data);
  const { view } = useAgentsSurface();
  const [newIn, setNewIn] = useState<string | undefined>(undefined);
  const [adding, setAdding] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const canvas = useRef<CanvasHandle>(null);
  const pendingFocus = useRef<string | null>(initialFocus ?? null);

  const threadOpen = view.kind === "thread" ? data.threads.get(view.thread) : undefined;
  // A conversation that's gone (deleted, or never loaded here) falls back to the canvas.
  const gone = view.kind === "thread" && !threadOpen && snap.conn === "live";
  useEffect(() => {
    if (gone) setAgentsView({ kind: "canvas" });
  }, [gone]);
  const shown: AgentsView = gone ? { kind: "canvas" } : view;

  // Detail for the open conversation, and for busy threads so their subagents appear.
  useEffect(() => {
    const want = new Set<string>();
    if (shown.kind === "thread") want.add(shown.thread);
    let busy = 0;
    for (const r of data.threads.values()) {
      if (isBusy(r.value.status) && busy < MAX_FOLLOWED_BUSY) {
        want.add(r.value.id);
        busy++;
      }
    }
    client.follow("surface", want);
  }, [client, data.threads, shown]);
  useEffect(() => () => client.unfollow("surface"), [client]);

  const openThread = useCallback((thread: string) => setAgentsView({ kind: "thread", thread }), []);
  const close = useCallback(() => setAgentsView({ kind: "canvas" }), []);
  const startNew = useCallback((project?: string) => {
    setNewIn(project);
    setAgentsView({ kind: "new" });
  }, []);
  const focus = useCallback(
    (key: string) => {
      if (key.startsWith("t:")) return openThread(key.slice(2));
      pendingFocus.current = key;
      setAgentsView({ kind: "canvas" });
      // Again once the canvas has the node (and has measured it).
      for (const ms of [80, 420]) setTimeout(() => canvas.current?.focus(key), ms);
    },
    [openThread],
  );
  // The desktop app's Agents window: status for the tray, and its links.
  useDesktop(chrome === "window" && !demo ? data : null, focus);
  useEffect(
    () =>
      chrome === "window"
        ? onDesktopDeepLink((link) => {
            if (link.kind === "agents") setAgentsView({ kind: "canvas" });
            else if (link.kind === "new") startNew();
          })
        : undefined,
    [chrome, startNew],
  );

  const ctx: AgentsCtx = useMemo(
    () => ({ client, owner: client.owner, demo, openThread, close, startNew, focus, services, sandboxes, phone }),
    [client, demo, openThread, close, startNew, focus, services, sandboxes, phone],
  );

  // Escape: back to the canvas, and from the canvas out of the popup. Not while typing, except in
  // an empty composer (it has focus in a conversation, so Escape would never leave otherwise).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      const el = e.target as HTMLElement | null;
      const typing = el && (el.isContentEditable || /^(INPUT|SELECT)$/.test(el.tagName) || (el.tagName === "TEXTAREA" && (el as HTMLTextAreaElement).value !== ""));
      if (typing || el?.closest(".xterm, [role=dialog]:not([data-surface])")) return;
      if (shown.kind !== "canvas") close();
      else onClose?.();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shown.kind, close, onClose]);

  const onSandbox = useCallback(
    (m: { id: string }) => {
      setAdding(false);
      focus(`m:${m.id}`);
    },
    [focus],
  );

  const { busy: working, requests } = rolling(data);
  // Waiting on you isn't running: those show as the waiting count.
  const running = working.filter((t) => t.status !== "waiting").length;
  const archived = [...data.threads.values()].filter((r) => r.value.archived_at !== null).length;
  const back = (
    <IconButton label="All agents (Esc)" onClick={close}>
      <ArrowLeft className="size-4" />
    </IconButton>
  );
  const end = (
    <>
      {shown.kind !== "new" && (
        <button onClick={() => startNew()} className="inline-flex h-7 items-center gap-1 rounded-md bg-fg px-2 text-[12px] font-medium text-bg transition hover:opacity-85" title="New agent">
          <Plus className="size-3.5" strokeWidth={2} />
          <span className="max-sm:sr-only">New agent</span>
        </button>
      )}
      {onClose && (
        <IconButton label="Close (Esc)" onClick={onClose}>
          <X className="size-4" />
        </IconButton>
      )}
    </>
  );

  return (
    <AgentsContext.Provider value={ctx}>
      <div className="relative flex h-full min-h-0 flex-col bg-bg text-fg">
        {/* The full window drives the logo itself (in the popup, the header already does). */}
        {chrome === "window" && <MarkDriver client={client} />}
        {shown.kind === "thread" && threadOpen ? (
          <Conversation thread={threadOpen.value} machine={data.machines.get(threadOpen.machine)?.value} start={back} end={end} />
        ) : shown.kind === "new" ? (
          <NewAgent key={newIn ?? ""} project={newIn} start={back} end={end} />
        ) : null}
        {/* The canvas stays mounted under a conversation, so going back is instant and keeps the view. */}
        <div className={`min-h-0 flex-1 flex-col ${shown.kind === "canvas" ? "flex" : "hidden"}`}>
          <div className="flex h-12 shrink-0 items-center gap-2 border-b border-line px-3" data-drag-region>
            {chrome === "window" && <WindowHome />}
            <h2 className="text-[13.5px] font-semibold tracking-[-0.02em]">Agents</h2>
            <span className="flex min-w-0 items-center gap-2 text-[12px] text-subtle">
              <span className={`size-1.5 shrink-0 rounded-full ${snap.conn === "live" ? (running ? "pulse-dot bg-accent" : "bg-ok") : snap.conn === "error" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
              <span className="truncate">
                {snap.conn === "live" ? (running ? `${running} running` : "All idle") : snap.conn === "error" ? (snap.error ?? "Can't reach roll") : "Reconnecting…"}
                {demo && " · demo"}
              </span>
              {requests.length > 0 && (
                <button onClick={() => openThread(requests[0].thread)} className="shrink-0 rounded-full bg-accent/15 px-2 py-0.5 text-[11.5px] text-accent hover:bg-accent/25" data-nodrag>
                  {requests.length} waiting for you
                </button>
              )}
            </span>
            <span className="ml-auto flex shrink-0 items-center gap-1" data-nodrag>
              <IconButton label="Add a machine" onClick={() => setAdding(true)}>
                <Server className="size-4" />
              </IconButton>
              <Menu
                label="More"
                trigger={<MoreHorizontal className="size-4" />}
                items={[
                  archived > 0 && { label: `${showArchived ? "Hide" : "Show"} archived (${archived})`, icon: <Archive />, onSelect: () => setShowArchived((s) => !s) },
                  !demo && { label: "Shared links", icon: <Share2 />, onSelect: () => window.location.assign(`/dashboard/agents/shares?owner=${encodeURIComponent(client.owner)}`) },
                ]}
              />
              {end}
            </span>
          </div>
          <div className="relative min-h-0 flex-1">
            {data.machines.size === 0 && snap.conn !== "connecting" ? (
              <Empty demo={demo} onCreated={onSandbox} />
            ) : (
              <CanvasView canvasRef={canvas} data={data} showArchived={showArchived} storageKey={client.owner} pendingFocus={pendingFocus} visible={shown.kind === "canvas"} />
            )}
            {phone && shown.kind === "canvas" && requests.length > 0 && <PhoneWaiting client={client} />}
          </div>
        </div>
        {adding && <AddMachineDialog onClose={() => setAdding(false)} onCreated={onSandbox} />}
      </div>
    </AgentsContext.Provider>
  );
}

function MarkDriver({ client }: { client: AgentsClient }) {
  useMarkDriver(useAgentsSnapshot(client));
  return null;
}

/** The full window's way back to the rest of roll. */
function WindowHome() {
  const { state } = useMarkState();
  return (
    <Link href="/dashboard" className="mr-1 flex items-center" aria-label="roll" title="Open roll" data-nodrag>
      <RollLogo state={state} size={16} />
    </Link>
  );
}

function CanvasView({
  canvasRef,
  data,
  showArchived,
  storageKey,
  pendingFocus,
  visible,
}: {
  canvasRef: React.RefObject<CanvasHandle | null>;
  data: AgentsClient["data"];
  showArchived: boolean;
  storageKey: string;
  pendingFocus: React.RefObject<string | null>;
  visible: boolean;
}) {
  const graph = useMemo(() => deriveGraph(data, { showArchived }), [data, showArchived]);
  // Show a node asked for before the canvas had it (a deep link, a new sandbox).
  useEffect(() => {
    const k = pendingFocus.current;
    if (!k || !visible || !graph.nodes.has(k)) return;
    pendingFocus.current = null;
    for (const ms of [60, 420]) setTimeout(() => canvasRef.current?.focus(k), ms);
  });
  const machineOf = (n: GraphNode) => data.machines.get(n.machine)?.value;
  const render = (n: GraphNode) => {
    switch (n.kind) {
      case "machine":
        return <MachineNode machine={data.machines.get(n.id)!.value} />;
      case "project":
        return <ProjectNode project={data.projects.get(n.id)!.value} machine={machineOf(n)} />;
      case "thread":
        return <ThreadNode thread={data.threads.get(n.id)!.value} machine={machineOf(n)} />;
      case "subagent": {
        const node = [...data.nodes.values()].map((m) => m.get(n.id)).find(Boolean)?.value;
        return node ? <SubagentNode node={node} /> : null;
      }
      case "request":
        return <RequestNode request={data.requests.get(n.id)!.value} />;
      case "terminal":
        return <TerminalNode terminal={data.terminals.get(n.id)!.value} machine={machineOf(n)} />;
    }
  };
  return <AgentsCanvas ref={canvasRef} graph={graph} storageKey={storageKey} render={render} defaultSize={defaultSize} active={visible} />;
}

function defaultSize(n: GraphNode): Size {
  switch (n.kind) {
    case "machine":
      return { w: 272, h: 120 };
    case "project":
      return { w: 256, h: 64 };
    case "thread":
      return { w: 296, h: 96 };
    case "subagent":
      return { w: 240, h: 72 };
    case "request":
      return { w: 320, h: 150 };
    case "terminal":
      return { w: 620, h: 380 };
  }
}

/** A phone: the oldest request waiting for an answer, answerable right here with one tap. */
function PhoneWaiting({ client }: { client: AgentsClient }) {
  const data = client.data;
  const { requests } = rolling(data);
  const q = requests[0];
  const t = data.threads.get(q.thread);
  return (
    <div className="rise absolute inset-x-2 bottom-[max(0.5rem,env(safe-area-inset-bottom))] z-20 max-h-[55%] overflow-y-auto rounded-xl border border-accent/40 bg-panel px-3.5 py-3" aria-label="Waiting for you">
      <RequestCard
        request={q}
        compact
        context={
          <button onClick={() => setAgentsView({ kind: "thread", thread: q.thread })} className="truncate hover:text-fg">
            {t?.value.title}
            {requests.length > 1 && <span className="ml-1.5 font-mono text-accent">+{requests.length - 1}</span>}
          </button>
        }
        onRespond={(response) => client.run(t!.machine, { type: "request.respond", request: q.id, response })}
      />
    </div>
  );
}

/**
 * No machines yet: the globe, big and alive, and the two ways in. The page scrolls on a short
 * screen; the globe follows the pointer and ripples on a click.
 */
function Empty({ demo, onCreated }: { demo: boolean; onCreated: (m: { id: string }) => void }) {
  const phone = usePhone();
  return (
    <section className="relative isolate h-full overflow-y-auto" aria-label="Get started">
      <DotField className="pointer-events-none absolute inset-0 -z-10 text-fg" />
      <div className="mx-auto flex min-h-full w-[min(760px,calc(100%-1.5rem))] flex-col items-center justify-center gap-5 py-10 text-center">
        <RollMark size={phone ? 140 : 200} state="idle" interactive />
        <div className="space-y-2">
          <h1 className="text-[28px] leading-tight font-semibold tracking-[-0.05em] sm:text-[32px]">Your agents, on one canvas</h1>
          <p className="mx-auto max-w-md text-[13.5px] leading-relaxed text-muted">
            Run Claude Code, Codex and other agents on your computer or on a sandbox roll hosts, and drive every one of them from here, on any screen.
          </p>
        </div>
        <AddMachinePaths onCreated={onCreated} />
        {!demo && (
          <Link href="/dashboard?demo#agents" className="text-[12.5px] text-subtle hover:text-fg">
            Try the demo first
          </Link>
        )}
      </div>
    </section>
  );
}
