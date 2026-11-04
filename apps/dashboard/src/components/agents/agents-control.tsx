import { Plus } from "lucide-react";
import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, type Transition } from "motion/react";
import { useEffect, useLayoutEffect, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { createPortal } from "react-dom";
import { DRAWER, FADE_IN, POP_OUT } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";
import { activeClient, DEMO_OWNER, type AgentsClient } from "@/lib/agents-client";
import { rolling } from "@/lib/agents-graph";
import { agentsSurface, hashFor, hideAgents, isNewAgentKey, showAgents, useAgentsSurface, viewFromHash, writeAgentsUrl } from "@/lib/agents-surface";
import { useAgentNotifications } from "@/lib/agents-notify";
import type { AgentsServices } from "@/lib/agents-services";
import { desktop, useDesktop } from "@/lib/desktop";
import { useOwnerSettings } from "@/lib/owner-settings";
import { AgentsSurface, CenterNote } from "./agents-page";
import { NoOwner } from "./no-owner";
import { SignIn } from "../code/sign-in";
import { useAgentsSnapshot, useHold, useMarkDriver } from "./use-agents";
import { useAgentsConnection, useDemoParam } from "./use-connection";

/**
 * The agents in the header of every dashboard page: how many are running and waiting, and "New
 * agent". Either opens the agents popup over the page (in the desktop app, the Agents window).
 * ⌘J toggles it, ⌘, opens the agents settings; `#agents`, `#agents/new` and `#agents/<thread>`
 * open it from a link. It also drives the shared logo (thinking while anything runs, success when
 * a run completes) and, by the owner's settings, the browser's notifications.
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
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey) return;
      if (e.key === ",") {
        // T3's ⌘, (its desktop menu's "Settings…"): the agents settings, beside the canvas.
        e.preventDefault();
        showAgents({ kind: "settings", target: "owner" });
        return;
      }
      if (e.key.toLowerCase() !== "j") return;
      e.preventDefault();
      if (agentsSurface.get().open) hideAgents();
      else showAgents({ kind: "canvas" });
    };
    const onHash = () => {
      const v = viewFromHash(window.location.hash);
      if (v) showAgents(v);
      // Back past the popup's entry: it closes.
      else if (agentsSurface.get().open) hideAgents();
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
      {client ? <Counts client={client} services={conn.services ?? undefined} /> : <Pill count={0} waiting={0} />}
      <Popup client={conn.client} services={conn.services} error={conn.error} need={conn.need} admin={conn.admin} demo={demo} />
    </>
  );
}

function Counts({ client, services }: { client: AgentsClient; services: AgentsServices | undefined }) {
  useHold(client);
  const snap = useAgentsSnapshot(client);
  useMarkDriver(snap);
  // Notifications by the owner's settings, from every page (not only while the popup is open).
  const prefs = useOwnerSettings(services?.settings, client.owner, snap?.data.settings?.revision ?? 0);
  useAgentNotifications(snap?.data ?? null, prefs.settings.notifications, (thread) => showAgents({ kind: "thread", thread }));
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
function Popup({ client, services, error, need, admin, demo }: Pick<ReturnType<typeof useAgentsConnection>, "client" | "services" | "error" | "need" | "admin"> & { demo: boolean }) {
  const { open, view } = useAgentsSurface();

  // The URL says what's open, so a reload (or a copied link) comes back to it; opening the popup is
  // a history entry, so Back closes it.
  const wasOpen = useRef(false);
  useEffect(() => {
    const was = wasOpen.current;
    wasOpen.current = open;
    const { hash, pathname, search } = window.location;
    const want = open ? hashFor(view) : "";
    const shown = viewFromHash(hash);
    // Already showing it; a query after the view (`?linear=connected`) stays for whoever reads it.
    if (open ? shown !== null && hashFor(shown) === want : !shown) return;
    writeAgentsUrl(pathname + search + want, open ? 1 : 0, was ? 1 : 0);
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

  // Portaled only once mounted: the server renders nothing here.
  const [mounted, setMounted] = useState(false);
  useEffect(() => setMounted(true), []);
  if (!mounted) return null;
  return createPortal(
    <AnimatePresence>
      {open && (
        <PopupFrame key="agents">
          {client && services ? (
            <AgentsSurface client={client} services={services} demo={demo} admin={admin} chrome="popup" onClose={hideAgents} />
          ) : (
            <CenterNote shimmer={!error} action={need === "owner" ? <NoOwner /> : need === "sign-in" ? <SignIn /> : undefined}>
              {error ?? "Connecting…"}
            </CenterNote>
          )}
        </PopupFrame>
      )}
    </AnimatePresence>,
    document.body,
  );
}

/**
 * The popup's frame: in as `.sheet-in` was (up 10px from 0.985 with a fade, on the drawer curve),
 * out the same way over 170 ms however it closes (⌘J, Escape, ×, the scrim), inert and hidden from
 * assistive tech as it goes, so a click lands on the page behind at once; then it unmounts. Was
 * gone in a frame. Not the shared dialog: the surface portals its own layers (the palette, toasts),
 * which a modal dialog's focus trap would shut out.
 */
export function PopupFrame({ children }: { children: ReactNode }) {
  const reduced = useReducedMotion();
  const none: Transition = { duration: 0 };
  const fade = useEnteringValue(0, 1);
  const scale = useEnteringValue(0.985, 1);
  const y = useEnteringValue(10, 0);
  const scrim = useEnteringValue(0, 1);
  const present = usePresenceTracks(
    [
      { value: fade, shown: 1, hidden: 0, enter: reduced ? none : FADE_IN, leave: reduced ? none : POP_OUT },
      { value: scale, shown: 1, hidden: 0.985, enter: reduced ? none : DRAWER, leave: reduced ? none : POP_OUT },
      { value: y, shown: 0, hidden: 10, enter: reduced ? none : DRAWER, leave: reduced ? none : POP_OUT },
      { value: scrim, shown: 1, hidden: 0, enter: reduced ? none : FADE_IN, leave: reduced ? none : POP_OUT },
    ],
    String(reduced),
  );
  return (
    <div className={`fixed inset-0 z-50 ${present ? "" : "pointer-events-none"}`} data-state={present ? "open" : "closing"} inert={!present} aria-hidden={!present || undefined}>
      <motion.div className="absolute inset-0 bg-(--scrim) backdrop-blur-[2px]" style={{ opacity: scrim }} onMouseDown={hideAgents} aria-hidden />
      <motion.div role="dialog" aria-modal="true" aria-label="Agents" data-surface data-agents className="absolute inset-0 overflow-hidden bg-bg text-fg" style={{ opacity: fade, scale, y }}>
        {children}
      </motion.div>
    </div>
  );
}
