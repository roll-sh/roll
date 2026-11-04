"use client";

import { Plus } from "lucide-react";
import { useEffect, useLayoutEffect, useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { activeClient, DEMO_OWNER, type AgentsClient } from "@/lib/agents-client";
import { rolling } from "@/lib/agents-graph";
import { agentsSurface, hashFor, hideAgents, isNewAgentKey, showAgents, useAgentsSurface, viewFromHash } from "@/lib/agents-surface";
import { desktop, useDesktop } from "@/lib/desktop";
import { AgentsSurface, CenterNote } from "./agents-page";
import { useAgentsSnapshot, useHold, useMarkDriver } from "./use-agents";
import { useAgentsConnection, useDemoParam } from "./use-connection";

/**
 * The agents in the header of every dashboard page: how many are running and waiting, and "New
 * agent". Either opens the agents popup over the page (in the desktop app, the Agents window).
 * ⌘J toggles it; `#agents`, `#agents/new` and `#agents/<thread>` open it from a link. It also
 * drives the shared logo (thinking while anything runs, success when a run completes).
 */
export function AgentsControl() {
  const demo = useDemoParam();
  const conn = useAgentsConnection({ demo });
  // The page's own surface (`/dashboard/agents`) may show another owner: follow it.
  const active = useSyncExternalStore(activeClient.subscribe, activeClient.get, () => null);
  const client = active ?? conn.client;

  // ⌘J, and links that open the popup (before the popup writes its own state into the URL).
  useLayoutEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      // ⌘N / ⌘⇧O: a new agent from any page (the open surface handles it itself).
      if (isNewAgentKey(e) && !agentsSurface.get().open && !(e.target as HTMLElement | null)?.closest?.(".xterm")) {
        e.preventDefault();
        showAgents({ kind: "new" });
        return;
      }
      if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== "j") return;
      e.preventDefault();
      if (agentsSurface.get().open) hideAgents();
      else showAgents({ kind: "canvas" });
    };
    const onHash = () => {
      const v = viewFromHash(window.location.hash);
      if (v) showAgents(v);
    };
    onHash();
    window.addEventListener("keydown", onKey);
    window.addEventListener("hashchange", onHash);
    return () => {
      window.removeEventListener("keydown", onKey);
      window.removeEventListener("hashchange", onHash);
    };
  }, []);

  return (
    <>
      {client ? <Counts client={client} /> : <Pill count={0} waiting={0} />}
      <Popup client={conn.client} services={conn.services} error={conn.error} demo={demo} />
    </>
  );
}

function Counts({ client }: { client: AgentsClient }) {
  useHold(client);
  const snap = useAgentsSnapshot(client);
  useMarkDriver(snap);
  // The tray, badge and notifications of the desktop app's main window (a no-op in a browser).
  const inMain = typeof window !== "undefined" && desktop()?.window === "main";
  useDesktop(snap && inMain && client.owner !== DEMO_OWNER ? snap.data : null, (key) => key.startsWith("t:") && showAgents({ kind: "thread", thread: key.slice(2) }));
  if (!snap) return <Pill count={0} waiting={0} />;
  const { busy, requests } = rolling(snap.data);
  // Waiting on you isn't running: those show as the waiting count.
  const running = busy.filter((t) => t.status !== "waiting");
  return <Pill count={running.length} waiting={requests.length} first={requests[0]?.thread ?? running[0]?.id} />;
}

/** `● 3 running · 1 waiting | + New agent`. */
function Pill({ count, waiting, first }: { count: number; waiting: number; first?: string }) {
  const live = count > 0 || waiting > 0;
  return (
    <div className={`flex h-[30px] shrink-0 items-stretch overflow-hidden rounded-lg border text-[12px] transition-colors ${live ? "border-accent/40" : "border-line hover:border-line-2"}`}>
      <button
        onClick={() => showAgents(waiting > 0 && first ? { kind: "thread", thread: first } : { kind: "canvas" })}
        className={`flex items-center gap-2 px-2.5 whitespace-nowrap transition-colors hover:bg-panel-2 ${live ? "text-fg" : "text-muted hover:text-fg"}`}
        title={waiting > 0 ? "Answer what's waiting (⌘J for all agents)" : "Your agents (⌘J)"}
        aria-label={`Agents: ${count} running, ${waiting} waiting`}
      >
        <span className={`size-1.5 rounded-full ${count ? "pulse-dot bg-accent" : "bg-subtle"}`} aria-hidden />
        {count > 0 ? (
          <span>
            <span className="font-mono tabular-nums">{count}</span> running
          </span>
        ) : (
          <span>Agents</span>
        )}
        {waiting > 0 && (
          <span className="rounded-full bg-accent px-1.5 font-mono text-[10.5px] leading-[16px] font-semibold text-white tabular-nums" title={`${waiting} waiting for you`}>
            {waiting}
          </span>
        )}
      </button>
      <span className={`w-px ${live ? "bg-accent/30" : "bg-line"}`} aria-hidden />
      <button onClick={() => showAgents({ kind: "new" })} className="flex items-center gap-1 px-2.5 text-muted whitespace-nowrap transition-colors hover:bg-panel-2 hover:text-fg" title="Start a new agent" aria-label="New agent">
        <Plus className="size-3.5" />
        <span className="max-md:hidden">New agent</span>
      </button>
    </div>
  );
}

/** The agents over the page: the whole window, like an app of its own. */
function Popup({ client, services, error, demo }: Pick<ReturnType<typeof useAgentsConnection>, "client" | "services" | "error"> & { demo: boolean }) {
  const { open, view } = useAgentsSurface();

  // The URL says what's open, so a reload (or a copied link) comes back to it.
  useEffect(() => {
    const { hash, pathname, search } = window.location;
    const want = open ? hashFor(view) : "";
    if (open ? hash === want : !viewFromHash(hash)) return;
    window.history.replaceState(window.history.state, "", pathname + search + want);
  }, [open, view]);

  // The page behind doesn't scroll while it's open.
  useEffect(() => {
    if (!open) return;
    const prev = document.body.style.overflow;
    document.body.style.overflow = "hidden";
    return () => {
      document.body.style.overflow = prev;
    };
  }, [open]);

  if (!open) return null;
  return createPortal(
    <div className="fixed inset-0 z-50">
      <div className="fade-in absolute inset-0 bg-(--scrim) backdrop-blur-[2px]" onMouseDown={hideAgents} aria-hidden />
      <div
        role="dialog"
        aria-modal="true"
        aria-label="Agents"
        data-surface
        data-agents
        className="sheet-in absolute inset-0 overflow-hidden bg-bg text-fg"
      >
        {client && services ? (
          <AgentsSurface client={client} services={services} demo={demo} chrome="popup" onClose={hideAgents} />
        ) : (
          <CenterNote shimmer={!error}>{error ?? "Connecting…"}</CenterNote>
        )}
      </div>
    </div>,
    document.body,
  );
}
