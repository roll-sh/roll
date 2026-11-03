"use client";

import { DotField, RollMark } from "@roll/brand";
import { Archive, Plus, Share2 } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState, useSyncExternalStore } from "react";
import { codeApi } from "@/lib/code-api";
import { clientFor, DEMO_OWNER, FOCUS_EVENT, setActiveClient, type AgentsClient } from "@/lib/agents-client";
import { DemoOwner } from "@/lib/agents-demo";
import { deriveGraph, rolling, type GraphNode } from "@/lib/agents-graph";
import { httpServices, type AgentsServices } from "@/lib/agents-services";
import { useDesktop } from "@/lib/desktop";
import type { Size } from "@/lib/agents-layout";
import { btn } from "../ui";
import { AddMachineDialog, AddMachinePaths } from "./add-machine";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { MachineNode, ProjectNode, RequestNode, SubagentNode, ThreadNode } from "./nodes";
import { RequestCard } from "./request-card";
import { useSandboxes } from "./sandbox";
import { SESSION_H, SESSION_W, ThreadSession } from "./session";
import { TerminalNode } from "./terminal";
import { AgentsContext, useAgentsSnapshot, useHold, type AgentsCtx } from "./use-agents";

/** Threads whose detail the canvas follows besides the open ones, so their subagents show. */
const MAX_FOLLOWED_BUSY = 12;

const LAST_OWNER = "roll:agents:owner";

/**
 * Which owner the canvas shows: `?owner=`, else the signed-in owner, else the last one used here,
 * else the first owner the server lists (admin and open servers).
 */
function useOwner(param: string | undefined, demo: boolean) {
  const [owner, setOwner] = useState<string | null>(demo ? DEMO_OWNER : (param ?? null));
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    if (demo || param) return;
    let live = true;
    (async () => {
      try {
        const info = await codeApi.info();
        let o = info.auth?.caller.kind === "owner" ? info.auth.caller.owner : null;
        if (!o) o = localStorage.getItem(LAST_OWNER);
        if (!o) o = (await codeApi.owners())[0]?.name ?? null;
        if (live) setOwner(o);
        if (live && !o) setError("No roll code owner yet. Create one in Code first.");
      } catch (e) {
        if (live) setError(e instanceof Error ? e.message : String(e));
      }
    })();
    return () => {
      live = false;
    };
  }, [param, demo]);
  useEffect(() => {
    if (owner && !demo) localStorage.setItem(LAST_OWNER, owner);
  }, [owner, demo]);
  return { owner, error };
}

/** The demo's in-memory owner, shared by its client and its services. */
let demoOwner: DemoOwner | null = null;
const theDemo = () => (demoOwner ??= new DemoOwner());

export function AgentsPage({ demo = false, owner: ownerParam, focus: focusParam }: { demo?: boolean; owner?: string; focus?: string }) {
  const { owner, error } = useOwner(ownerParam, demo);
  const client = useMemo(() => {
    if (!owner) return null;
    return demo ? clientFor(DEMO_OWNER, theDemo) : clientFor(owner);
  }, [owner, demo]);
  const services = useMemo(() => (demo ? theDemo().services() : httpServices()), [demo]);
  useEffect(() => {
    setActiveClient(client);
    return () => setActiveClient(null);
  }, [client]);

  if (error && !client) return <CenterNote>{error}</CenterNote>;
  if (!client) return <CenterNote shimmer>Connecting…</CenterNote>;
  return <Canvas client={client} demo={demo} focusParam={focusParam} services={services} />;
}

const PHONE = "(max-width: 640px)";

/** A phone-sized screen (sessions open full screen there). */
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

/** The visible part of the page above an on-screen keyboard (`visualViewport`), for full-screen sheets. */
function useVisibleViewport() {
  const [v, setV] = useState<{ top: number; height: number } | null>(null);
  useEffect(() => {
    const vv = window.visualViewport;
    if (!vv) return;
    const on = () => setV({ top: vv.offsetTop, height: vv.height });
    on();
    vv.addEventListener("resize", on);
    vv.addEventListener("scroll", on);
    return () => {
      vv.removeEventListener("resize", on);
      vv.removeEventListener("scroll", on);
    };
  }, []);
  return v;
}

function CenterNote({ children, shimmer = false }: { children: React.ReactNode; shimmer?: boolean }) {
  return (
    <div className="grid h-full place-items-center text-[13px] text-subtle">
      <span className={shimmer ? "shimmer" : ""}>{children}</span>
    </div>
  );
}

function Canvas({ client, demo, focusParam, services }: { client: AgentsClient; demo: boolean; focusParam?: string; services: AgentsServices }) {
  useHold(client);
  const snap = useAgentsSnapshot(client)!;
  const data = snap.data;
  const phone = usePhone();
  const sandboxes = useSandboxes(services, client.owner, data);
  const [expanded, setExpandedSet] = useState<ReadonlySet<string>>(() => new Set());
  const [showArchived, setShowArchived] = useState(false);
  const [adding, setAdding] = useState(false);
  const canvas = useRef<CanvasHandle>(null);
  const order = useRef<string[]>([]);

  const graph = useMemo(() => deriveGraph(data, { expanded, showArchived }), [data, expanded, showArchived]);

  // Detail for open threads, and for busy ones so their subagents appear.
  useEffect(() => {
    const want = new Set(expanded);
    let busy = 0;
    for (const r of data.threads.values()) {
      const s = r.value.status;
      if ((s === "running" || s === "waiting" || s === "starting") && busy < MAX_FOLLOWED_BUSY) {
        want.add(r.value.id);
        busy++;
      }
    }
    client.follow("canvas", want);
  }, [client, data.threads, expanded]);
  useEffect(() => () => client.unfollow("canvas"), [client]);

  const pendingFocus = useRef<{ key: string; zoom?: number } | null>(null);
  // A phone shows one session at a time (full screen): opening one closes the others.
  const phoneRef = useRef(phone);
  phoneRef.current = phone;
  const setExpanded = useCallback((thread: string, open: boolean) => {
    const only = open && phoneRef.current;
    setExpandedSet((s) => {
      if (only) return s.size === 1 && s.has(thread) ? s : new Set([thread]);
      if (s.has(thread) === open) return s;
      const next = new Set(s);
      if (open) next.add(thread);
      else next.delete(thread);
      return next;
    });
    order.current = only ? [] : order.current.filter((t) => t !== thread);
    if (open) order.current.push(thread);
    pendingFocus.current = { key: `t:${thread}`, zoom: open ? 1 : undefined };
  }, []);

  // Focus once the layout has the node (after an expand, or a request from the header).
  useEffect(() => {
    const p = pendingFocus.current;
    if (!p || !graph.nodes.has(p.key)) return;
    pendingFocus.current = null;
    // Again once the opened card has been measured and the layout has made room for it.
    // (Not cleared on re-render: the stream re-renders this every frame.)
    for (const ms of [60, 420]) setTimeout(() => canvas.current?.focus(p.key, p.zoom), ms);
  });

  const focus = useCallback(
    (key: string) => {
      if (key.startsWith("t:")) setExpanded(key.slice(2), true);
      else {
        pendingFocus.current = { key };
        // Re-render to run the focus effect.
        setExpandedSet((s) => new Set(s));
      }
    },
    [setExpanded],
  );
  // The desktop app's tray, badge and notifications (a no-op in a browser).
  useDesktop(demo ? null : data, focus);

  useEffect(() => {
    const on = (e: Event) => focus(String((e as CustomEvent).detail));
    window.addEventListener(FOCUS_EVENT, on);
    return () => window.removeEventListener(FOCUS_EVENT, on);
  }, [focus]);
  const focusedParam = useRef(false);
  useEffect(() => {
    if (focusParam && !focusedParam.current && graph.nodes.has(focusParam)) {
      focusedParam.current = true;
      focus(focusParam);
    }
  }, [focusParam, graph, focus]);

  const ctx: AgentsCtx = useMemo(
    () => ({ client, owner: client.owner, demo, expanded, setExpanded, focus, services, sandboxes, phone }),
    [client, demo, expanded, setExpanded, focus, services, sandboxes, phone],
  );
  // A new sandbox is shown while it boots, and again once it connects (its card grows then).
  const booting = useRef<string | null>(null);
  const onSandbox = useCallback(
    (m: { id: string }) => {
      setAdding(false);
      booting.current = m.id;
      focus(`m:${m.id}`);
    },
    [focus],
  );
  const bootedOnline = booting.current ? data.machines.get(booting.current)?.value.online : false;
  useEffect(() => {
    if (!bootedOnline || !booting.current) return;
    const key = `m:${booting.current}`;
    booting.current = null;
    // After the card has been measured at its new size.
    setTimeout(() => focus(key), 300);
  }, [bootedOnline, focus]);
  // On a phone the newest open thread is a full-screen sheet; the canvas keeps its small card.
  const sheet = phone ? [...order.current].reverse().find((t) => expanded.has(t) && data.threads.has(t)) : undefined;

  const machineOf = (n: GraphNode) => data.machines.get(n.machine)?.value;
  const render = (n: GraphNode) => {
    switch (n.kind) {
      case "machine":
        return <MachineNode machine={data.machines.get(n.id)!.value} />;
      case "project":
        return <ProjectNode project={data.projects.get(n.id)!.value} machine={machineOf(n)} />;
      case "thread": {
        const t = data.threads.get(n.id)!.value;
        return expanded.has(t.id) && !phone ? <ThreadSession thread={t} machine={machineOf(n)} /> : <ThreadNode thread={t} machine={machineOf(n)} />;
      }
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

  const defaultSize = useCallback(
    (n: GraphNode): Size => {
      switch (n.kind) {
        case "machine":
          return { w: 288, h: 210 };
        case "project":
          return { w: 272, h: 112 };
        case "thread":
          return expanded.has(n.id) && !phone ? { w: SESSION_W, h: SESSION_H } : { w: 304, h: 118 };
        case "subagent":
          return { w: 248, h: 84 };
        case "request":
          return { w: 340, h: 200 };
        case "terminal":
          return { w: 620, h: 380 };
      }
    },
    [expanded, phone],
  );

  const onEscape = useCallback(() => {
    const last = order.current[order.current.length - 1];
    if (last) setExpanded(last, false);
  }, [setExpanded]);

  const running = [...data.threads.values()].filter((r) => ["running", "waiting", "starting"].includes(r.value.status)).length;
  const archived = [...data.threads.values()].filter((r) => r.value.archived_at !== null).length;
  const empty = data.machines.size === 0 && snap.conn !== "connecting";
  const waiting = rolling(data).requests;

  return (
    <AgentsContext.Provider value={ctx}>
      <div className="relative h-full">
        {empty ? (
          <Empty demo={demo} onCreated={onSandbox} />
        ) : (
          <AgentsCanvas
            ref={canvas}
            graph={graph}
            storageKey={client.owner}
            render={render}
            defaultSize={defaultSize}
            onEscape={onEscape}
            overlay={
              <div className="pointer-events-none absolute top-3 right-3 left-3 flex flex-wrap items-center gap-2 sm:top-4 sm:right-auto sm:left-4">
                <div className="pointer-events-auto flex min-w-0 items-center gap-2.5 rounded-lg border border-line bg-panel/90 px-3 py-1.5 text-[12px] backdrop-blur">
                  <span className={`size-1.5 rounded-full ${snap.conn === "live" ? (running ? "bg-accent" : "bg-ok") : snap.conn === "error" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
                  <span className="font-medium">{demo ? "demo" : client.owner}</span>
                  <span className="truncate text-subtle">
                    {snap.conn === "live"
                      ? `${data.machines.size} machine${data.machines.size === 1 ? "" : "s"} · ${running} rolling`
                      : snap.conn === "error"
                        ? (snap.error ?? "Can't reach roll")
                        : "Reconnecting…"}
                  </span>
                </div>
                {archived > 0 && (
                  <button
                    onClick={() => setShowArchived((s) => !s)}
                    aria-pressed={showArchived}
                    className={`pointer-events-auto ${btn} bg-panel/90 backdrop-blur ${showArchived ? "text-fg" : ""}`}
                  >
                    <Archive className="size-3.5" />
                    <span className="max-sm:sr-only">
                      {showArchived ? "Hide" : "Show"} archived ({archived})
                    </span>
                  </button>
                )}
                <button onClick={() => setAdding(true)} className={`pointer-events-auto ${btn} bg-panel/90 backdrop-blur`} aria-label="Add machine">
                  <Plus className="size-3.5" /> <span className="max-sm:sr-only">Add machine</span>
                </button>
                {!demo && (
                  <Link
                    href={`/dashboard/agents/shares?owner=${encodeURIComponent(client.owner)}`}
                    className={`pointer-events-auto ${btn} bg-panel/90 backdrop-blur`}
                    title="Links to shared threads"
                    aria-label="Shares"
                  >
                    <Share2 className="size-3.5" /> <span className="max-sm:sr-only">Shares</span>
                  </Link>
                )}
              </div>
            }
          />
        )}
        {adding && <AddMachineDialog onClose={() => setAdding(false)} onCreated={onSandbox} />}
        {phone && !sheet && waiting.length > 0 && <PhoneWaiting client={client} />}
        {sheet && <PhoneSheet thread={sheet} data={data} />}
      </div>
    </AgentsContext.Provider>
  );
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
          <button onClick={() => window.dispatchEvent(new CustomEvent(FOCUS_EVENT, { detail: `t:${q.thread}` }))} className="truncate hover:text-fg">
            {t?.value.title}
            {requests.length > 1 && <span className="ml-1.5 font-mono text-accent">+{requests.length - 1}</span>}
          </button>
        }
        onRespond={(response) => client.run(t!.machine, { type: "request.respond", request: q.id, response })}
      />
    </div>
  );
}

/** A thread open on a phone: full screen, sized to what the keyboard leaves visible. */
function PhoneSheet({ thread, data }: { thread: string; data: AgentsCtx["client"]["data"] }) {
  const vv = useVisibleViewport();
  const r = data.threads.get(thread)!;
  return (
    <div className="fixed inset-x-0 z-50" style={vv ? { top: vv.top, height: vv.height } : { top: 0, height: "100dvh" }}>
      <ThreadSession thread={r.value} machine={data.machines.get(r.machine)?.value} fullscreen />
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
      <DotField className="pointer-events-none fixed inset-0 -z-10 text-fg" />
      <div className="mx-auto flex min-h-full w-[min(760px,calc(100%-1.5rem))] flex-col items-center justify-center gap-5 py-10 text-center">
        <RollMark size={phone ? 150 : 220} state="idle" interactive />
        <div className="space-y-2">
          <h1 className="text-[28px] leading-tight font-semibold tracking-[-0.05em] sm:text-[34px]">Your agents, on one canvas</h1>
          <p className="mx-auto max-w-md text-[13.5px] leading-relaxed text-muted">
            Run Claude Code, Codex and other agents on your computer or on a sandbox roll hosts, and drive every one of them from here, on any screen.
          </p>
        </div>
        <AddMachinePaths onCreated={onCreated} />
        {!demo && (
          <Link href="/dashboard/agents?demo" className={`${btn} bg-panel/80`}>
            Try the demo first
          </Link>
        )}
      </div>
    </section>
  );
}
