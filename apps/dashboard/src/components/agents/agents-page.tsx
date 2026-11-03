"use client";

import { DotField, RollMark } from "@roll/brand";
import { Archive, Plus } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { codeApi } from "@/lib/code-api";
import { clientFor, DEMO_OWNER, FOCUS_EVENT, setActiveClient, type AgentsClient } from "@/lib/agents-client";
import { DemoOwner } from "@/lib/agents-demo";
import { deriveGraph, type GraphNode } from "@/lib/agents-graph";
import { useDesktop } from "@/lib/desktop";
import type { Size } from "@/lib/agents-layout";
import { btn, primary } from "../ui";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { MachineNode, ProjectNode, RequestNode, SubagentNode, ThreadNode } from "./nodes";
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

export function AgentsPage({ demo = false, owner: ownerParam, focus: focusParam }: { demo?: boolean; owner?: string; focus?: string }) {
  const { owner, error } = useOwner(ownerParam, demo);
  const client = useMemo(() => {
    if (!owner) return null;
    return demo ? clientFor(DEMO_OWNER, () => new DemoOwner()) : clientFor(owner);
  }, [owner, demo]);
  useEffect(() => {
    setActiveClient(client);
    return () => setActiveClient(null);
  }, [client]);

  if (error && !client) return <CenterNote>{error}</CenterNote>;
  if (!client) return <CenterNote shimmer>Connecting…</CenterNote>;
  return <Canvas client={client} demo={demo} focusParam={focusParam} />;
}

function CenterNote({ children, shimmer = false }: { children: React.ReactNode; shimmer?: boolean }) {
  return (
    <div className="grid h-full place-items-center text-[13px] text-subtle">
      <span className={shimmer ? "shimmer" : ""}>{children}</span>
    </div>
  );
}

function Canvas({ client, demo, focusParam }: { client: AgentsClient; demo: boolean; focusParam?: string }) {
  useHold(client);
  const snap = useAgentsSnapshot(client)!;
  const data = snap.data;
  const [expanded, setExpandedSet] = useState<ReadonlySet<string>>(() => new Set());
  const [showArchived, setShowArchived] = useState(false);
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
  const setExpanded = useCallback((thread: string, open: boolean) => {
    setExpandedSet((s) => {
      if (s.has(thread) === open) return s;
      const next = new Set(s);
      if (open) next.add(thread);
      else next.delete(thread);
      return next;
    });
    order.current = order.current.filter((t) => t !== thread);
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
    () => ({ client, owner: client.owner, demo, expanded, setExpanded, focus }),
    [client, demo, expanded, setExpanded, focus],
  );

  const machineOf = (n: GraphNode) => data.machines.get(n.machine)?.value;
  const render = (n: GraphNode) => {
    switch (n.kind) {
      case "machine":
        return <MachineNode machine={data.machines.get(n.id)!.value} />;
      case "project":
        return <ProjectNode project={data.projects.get(n.id)!.value} machine={machineOf(n)} />;
      case "thread": {
        const t = data.threads.get(n.id)!.value;
        return expanded.has(t.id) ? <ThreadSession thread={t} machine={machineOf(n)} /> : <ThreadNode thread={t} machine={machineOf(n)} />;
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
          return expanded.has(n.id) ? { w: SESSION_W, h: SESSION_H } : { w: 304, h: 118 };
        case "subagent":
          return { w: 248, h: 84 };
        case "request":
          return { w: 340, h: 200 };
        case "terminal":
          return { w: 620, h: 380 };
      }
    },
    [expanded],
  );

  const onEscape = useCallback(() => {
    const last = order.current[order.current.length - 1];
    if (last) setExpanded(last, false);
  }, [setExpanded]);

  const running = [...data.threads.values()].filter((r) => ["running", "waiting", "starting"].includes(r.value.status)).length;
  const archived = [...data.threads.values()].filter((r) => r.value.archived_at !== null).length;
  const empty = data.machines.size === 0 && snap.conn !== "connecting";

  return (
    <AgentsContext.Provider value={ctx}>
      <div className="relative h-full">
        {empty ? (
          <Empty demo={demo} />
        ) : (
          <AgentsCanvas
            ref={canvas}
            graph={graph}
            storageKey={client.owner}
            render={render}
            defaultSize={defaultSize}
            onEscape={onEscape}
            overlay={
              <div className="pointer-events-none absolute top-4 left-4 flex items-center gap-2">
                <div className="pointer-events-auto flex items-center gap-2.5 rounded-lg border border-line bg-panel/90 px-3 py-1.5 text-[12px] backdrop-blur">
                  <span className={`size-1.5 rounded-full ${snap.conn === "live" ? (running ? "bg-accent" : "bg-ok") : snap.conn === "error" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
                  <span className="font-medium">{demo ? "demo" : client.owner}</span>
                  <span className="text-subtle">
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
                    <Archive className="size-3.5" /> {showArchived ? "Hide" : "Show"} archived ({archived})
                  </button>
                )}
                {!demo && (
                  <Link href="/dashboard/agents/connect" className={`pointer-events-auto ${btn} bg-panel/90 backdrop-blur`}>
                    <Plus className="size-3.5" /> Add machine
                  </Link>
                )}
              </div>
            }
          />
        )}
      </div>
    </AgentsContext.Provider>
  );
}

function Empty({ demo }: { demo: boolean }) {
  return (
    <section className="relative isolate grid h-full place-items-center overflow-hidden">
      <DotField className="-z-10 text-fg" />
      <div className="flex max-w-md flex-col items-center gap-4 text-center">
        <RollMark size={120} state="sleep" interactive />
        <h1 className="text-2xl font-semibold tracking-[-0.045em]">Connect a machine</h1>
        <p className="text-[13.5px] leading-relaxed text-muted">
          Run your coding agents on your own machines and drive them all from this canvas. On the machine, run:
        </p>
        <pre className="rounded-lg border border-line bg-panel px-4 py-2.5 font-mono text-[12.5px]">roll agent login</pre>
        <div className="flex gap-2">
          <Link href="/dashboard/agents/connect" className={primary}>
            <Plus className="size-3.5" /> Add machine
          </Link>
          {!demo && (
            <Link href="/dashboard/agents?demo" className={btn}>
              Try the demo
            </Link>
          )}
        </div>
      </div>
    </section>
  );
}
