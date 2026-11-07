"use client";

import { DotField, HorizonMark, RollMark, useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, useIsPresent } from "motion/react";
import { Archive, ArrowLeft, ChevronDown, ChevronUp, MoreHorizontal, PanelRightClose, Pin, PinOff, Plus, Search, Server, Settings, Share2, X, Zap } from "lucide-react";
import Link from "next/link";
import { memo, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { isBusy, reachable, type Thread } from "@/lib/agents-api";
import { POP_IN, POP_OUT } from "@/lib/agents-motion";
import { setActiveClient, type AgentsClient } from "@/lib/agents-client";
import { collapseUnused, deriveGraph, foldQuiet, groupByStatus, newestRuns, rolling, sameGraph, SHOWN_IN_GROUP, SHOWN_THREADS, type Graph, type GraphNode } from "@/lib/agents-graph";
import { emptyData, runsOf, type AgentsData } from "@/lib/agents-store";
import type { Direction, Size } from "@/lib/agents-layout";
import { closePalette, noteThreadOpen, openPalette, togglePalette } from "@/lib/agents-palette";
import { useKeyboard } from "@/lib/keyboard-inset";
import { lastInputWasKeyboard } from "@/lib/input-modality";
import type { AgentsServices } from "@/lib/agents-services";
import { hashFor, isNewAgentKey, newAgentHint, setAgentsView, threadOfView, useAgentsSurface, viewFromHash, writeAgentsUrl, type AgentsView, type AutomationsView } from "@/lib/agents-surface";
import { isBuilder } from "@/lib/automations-nav";
import { ALL, applyDisplay, applyView, deleteView, directionOf, focusOn, isPinned, keepProject, loadFocused, pinKey, setFocused as setFocusedIn, pinnedConversations, pinOf, projectOf, viewStorageKey, withDirection, type Pin as PinRec } from "@/lib/agents-views";
import { desktop, onDesktopDeepLink, useDesktop } from "@/lib/desktop";
import { LINK_PARAMS, type IntegrationsLink } from "@/lib/integrations-api";
import { useModelPrefsSync, useOwnerSettings } from "@/lib/owner-settings";
import { useMarkState, useMarkStateValue } from "../mark-state";
import { SignIn } from "../code/sign-in";
import { AddMachineDialog, AddMachinePaths } from "./add-machine";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { AutomationsSheet } from "./automations/sheet";
import { Notices, useUnseenNotices } from "./automations/notices";
import { CommandPalette, type PaletteHost } from "./command-palette";
import { openIntegrations } from "./integrations";
import { Menu } from "./menu";
import { Launcher } from "./new-agent";
import { useKept } from "./ui/dialog";
import { AgentsLayer } from "./ui/layer";
import { PinnedConversation, PinnedDiff, PinnedTerminal } from "./pinned";
import { MachineNode, ProjectNode, RequestNode, SubagentNode, ThreadCard, useThreadFacts, type ThreadFacts } from "./nodes";
import { CanvasProjectsProvider, type CanvasProjects } from "./machine-projects";
import { CanvasFoldsProvider, type CanvasFolds } from "./canvas-folds";
import { FoldedThreads, StatusHeader } from "./folded-threads";
import type { StatusGroup } from "@/lib/agents-graph";
import { ProjectDialogs } from "./project-actions";
import { PromptBar } from "./prompt-bar";
import { RightDock } from "./right-dock";
import { PendingRequest } from "./pending-request";
import { SettingsPanel } from "./settings-panel";
import { useSandboxes } from "./sandbox";
import { Conversation, IconButton } from "./session";
import { closeTopLayer } from "./escape-layers";
import { openSettings } from "./provider-setup";
import { useFocusRescue } from "./focus-rescue";
import { CloseTerminal, TerminalNode } from "./terminal";
import { threadDone } from "./thread-status";
import { SidebarToggle, ThreadsSidebar, useSidebarOpen } from "./threads-sidebar";
import { AgentsContext, useAgents, useAgentsSnapshot, useHold, useMarkDriver, type AgentsCtx } from "./use-agents";
import { useAgentsConnection } from "./use-connection";
import { ADD_MACHINE_EVENT } from "./settings-ui";
import { OwnerSwitcher } from "./owner-switcher";
import { NoOwner } from "./no-owner";
import { PinTools, useCanvasViews, ViewsBar, type CanvasViews } from "./views";
import { BulkBar, SelectionHost, useBulk, type Bulk } from "./bulk-bar";
import { ThreadCollection, useCollection } from "./thread-list";
import { needsMe, nextOf, rowsOf } from "@/lib/agents-collection";
import { selection } from "@/lib/agents-selection";
import { layoutOf } from "@/lib/agents-views";
import { agentsSurface } from "@/lib/agents-surface";
import { showInboxToast } from "./threads-sidebar";

/** Threads whose detail the canvas follows besides the open one, so their subagents show. */
const MAX_FOLLOWED_BUSY = 12;

const PHONE = "(max-width: 640px)";

/** For a store that never changes (the platform). */
const noSubscribe = () => () => {};

let phoneQuery: { of: typeof window.matchMedia; mq: MediaQueryList } | null = null;
// One query list: a snapshot is read on every render of every card (`matchMedia` per render showed in profiles).
const phoneMq = () => {
  if (phoneQuery?.of !== window.matchMedia) phoneQuery = { of: window.matchMedia, mq: window.matchMedia(PHONE) };
  return phoneQuery.mq;
};
/** A phone-sized screen. */
export function usePhone(): boolean {
  return useSyncExternalStore(
    (fn) => {
      const mq = phoneMq();
      mq.addEventListener("change", fn);
      return () => mq.removeEventListener("change", fn);
    },
    () => phoneMq().matches,
    () => false,
  );
}

/**
 * `/dashboard/agents`: the agents surface filling the window. The desktop app's Agents window
 * shows this page; in a browser the header's popup is the usual way in.
 */
export function AgentsPage({
  demo = false,
  owner,
  focus,
  fresh = false,
  integrations,
}: {
  demo?: boolean;
  owner?: string;
  focus?: string;
  fresh?: boolean;
  /** Back from installing Slack or Discord (`?settings=integrations&…`): the settings, on the integrations. */
  integrations?: IntegrationsLink | null;
}) {
  const { client, services, error, need, admin } = useAgentsConnection({ owner, demo });
  // The link it opened with, read once: the URL below follows the view, so by a second run of the
  // effect (React's strict mode runs it twice) it already says the canvas, and a link to the
  // automations sheet fell back to it.
  const [linked] = useState(() => (typeof window === "undefined" ? null : viewFromHash(window.location.hash)));
  // Where the window opens (`?new`, `?focus=t:thr_…`, `?settings=integrations`, or a `#agents/…`
  // link as the popup takes them), before the first paint.
  useLayoutEffect(() => {
    if (integrations) {
      openIntegrations(integrations);
      // Shown once: a reload or a shared URL doesn't say it again.
      const url = new URL(window.location.href);
      for (const k of LINK_PARAMS) url.searchParams.delete(k);
      window.history.replaceState(window.history.state, "", url);
    } else if (fresh) setAgentsView({ kind: "new" });
    else if (focus?.startsWith("t:")) setAgentsView({ kind: "thread", thread: focus.slice(2) });
    else if (linked) setAgentsView(linked);
    // Only where it opens; later views are the user's.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);
  // A `#agents/…` link followed on this page (a notice's, a pasted one).
  useEffect(() => {
    const onHash = () => {
      const v = viewFromHash(window.location.hash);
      if (v) setAgentsView(v);
    };
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  // The URL says what's open, as the popup's does: a reload or a copied link comes back to it
  // (after Create, an automation's own page, not `…/new`).
  // A panel (a conversation, settings, the launcher) is a history entry over the canvas: Back closes it.
  const { view: shownView } = useAgentsSurface();
  const depth = shownView.kind === "canvas" ? 0 : 1;
  const wasDepth = useRef(0);
  useEffect(() => {
    const was = wasDepth.current;
    wasDepth.current = depth;
    const { hash, pathname, search } = window.location;
    const want = hashFor(shownView);
    const shown = viewFromHash(hash);
    if (shown !== null && hashFor(shown) === want) return;
    writeAgentsUrl(pathname + search + want, depth, was);
  }, [shownView, depth]);
  const mac = typeof window !== "undefined" && desktop()?.window === "agents" && desktop()?.platform === "darwin";
  return (
    <div className="h-dvh bg-bg text-fg" data-agents data-desktop={mac ? "agents-mac" : undefined}>
      {client && services ? (
        <AgentsSurface client={client} services={services} demo={demo} admin={admin} chrome="window" focus={focus && !focus.startsWith("t:") ? focus : undefined} />
      ) : (
        <CenterNote shimmer={!error} action={need === "owner" ? <NoOwner /> : error ? <SignIn /> : undefined}>
          {error ?? "Connecting…"}
        </CenterNote>
      )}
    </div>
  );
}

/** Connecting, or why it can't: the mark thinking (or still) over a line of text, and what to do about it. */
export function CenterNote({ children, shimmer = false, action }: { children: ReactNode; shimmer?: boolean; action?: ReactNode }) {
  return (
    <div className="fade-in grid h-full place-items-center px-6 text-center">
      <div className="flex flex-col items-center gap-4 text-fg">
        <HorizonMark size={40} state={shimmer ? "thinking" : "error"} label="" />
        <span className={`text-[13px] ${shimmer ? "shimmer" : "text-muted"}`}>{children}</span>
        {action}
      </div>
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
  admin: signedInAsAdmin = false,
}: {
  client: AgentsClient;
  services: AgentsServices;
  demo: boolean;
  chrome: "popup" | "window";
  onClose?: () => void;
  /** A canvas node to show first (`m:…`, `x:…`). */
  focus?: string;
  /** Signed in as admin: the header switches between owners (the dashboard's sidebar does too). */
  admin?: boolean;
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
  const admin = signedInAsAdmin && !demo;
  const { view } = useAgentsSurface();
  const [adding, setAdding] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const [sidebar, setSidebar] = useSidebarOpen(phone);
  const canvas = useRef<CanvasHandle>(null);
  // The prompt bar's dock: the canvas keeps cards clear of it.
  const dock = useRef<HTMLDivElement>(null);
  const views = useCanvasViews(services.canvas, client.owner, data.canvas?.revision ?? 0);
  const root = useRef<HTMLDivElement | null>(null);
  useFocusRescue(root);
  // The same element as state: the launcher portals into it.
  const [rootEl, setRootEl] = useState<HTMLDivElement | null>(null);
  const attachRoot = useCallback((el: HTMLDivElement | null) => {
    root.current = el;
    setRootEl(el);
  }, []);
  const pendingFocus = useRef<string | null>(initialFocus ?? null);

  const threadOpen = view.kind === "thread" ? data.threads.get(view.thread) : undefined;
  // A conversation that's gone (deleted, or never loaded here) falls back to the canvas.
  const gone = useGone(view.kind === "thread" ? view.thread : null, !!threadOpen, snap.conn === "live");
  useEffect(() => {
    if (gone) setAgentsView({ kind: "canvas" });
  }, [gone]);
  const shown: AgentsView = gone ? { kind: "canvas" } : view;
  // Kept while they play their way out.
  const launcher = useKept(shown.kind === "new" && shown);
  const addMachine = useKept(adding);
  // The card beside the open panel, selected on the canvas (a conversation opened from a run, in the automations sheet, too).
  const selected =
    shown.kind === "thread" ? `t:${shown.thread}` : shown.kind === "automations" && shown.thread ? `t:${shown.thread}` : shown.kind === "settings" && shown.target !== "owner" ? shown.target : undefined;
  // The automations sheet: what it highlights on the canvas (a run's threads), Back for Escape,
  // the builder maximized, and the sheet kept a beat after it closes so it can slide out.
  const [highlighted, setHighlighted] = useState<readonly string[]>([]);
  const runInSheet = useRef<{ view: AutomationsView; keys: readonly string[] } | null>(null);
  const sheetBack = useRef<(() => void) | null>(null);
  const [maximized, setMaximized] = useMaximized();
  const wide = !phone && maximized && shown.kind === "automations" && isBuilder(shown);
  const firstHighlight = highlighted[0];
  useEffect(() => {
    runInSheet.current = shown.kind === "automations" && shown.run && highlighted.length ? { view: shown, keys: highlighted } : null;
  }, [shown, highlighted]);
  useEffect(() => {
    if (!firstHighlight || selected) return;
    const t = setTimeout(() => canvas.current?.reveal(firstHighlight), 280);
    return () => clearTimeout(t);
  }, [firstHighlight, selected]);
  useEffect(() => {
    if (!selected) return;
    // Once the panel has made the canvas narrower.
    const t = setTimeout(() => canvas.current?.reveal(selected), 280);
    return () => clearTimeout(t);
  }, [selected]);

  // Detail for the open conversation, and for busy threads so their subagents appear.
  useEffect(() => {
    const want = new Set<string>();
    // A conversation opened over a run in the sheet too (it stayed "Loading the conversation…").
    const open = threadOfView(shown);
    if (open) want.add(open);
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

  // A run in the automations sheet and the cards it highlights: one of those cards opens its
  // conversation in the sheet, over the run (Back returns to it), not in place of the sheet.
  const openThread = useCallback((thread: string) => {
    // How often and how lately, for the command palette's ranking.
    noteThreadOpen(thread);
    const inSheet = runInSheet.current;
    if (inSheet?.keys.includes(`t:${thread}`)) setAgentsView({ ...inSheet.view, thread });
    else setAgentsView({ kind: "thread", thread });
  }, []);
  const close = useCallback(() => setAgentsView({ kind: "canvas" }), []);

  // Focus comes back to the canvas when a conversation, settings or the launcher closes from the
  // keyboard (it was lost with the panel): the card of what was open, else what had focus on the
  // canvas before. Not after a click (`lib/input-modality.ts`).
  const lastCanvasFocus = useRef<HTMLElement | null>(null);
  useEffect(() => {
    const on = (e: FocusEvent) => {
      const t = e.target as HTMLElement | null;
      if (t && root.current?.contains(t) && !t.closest("[data-panel], [role=dialog]:not([data-surface])")) lastCanvasFocus.current = t;
    };
    document.addEventListener("focusin", on);
    return () => document.removeEventListener("focusin", on);
  }, []);
  const wasShown = useRef<AgentsView>(shown);
  useEffect(() => {
    const before = wasShown.current;
    wasShown.current = shown;
    if (shown.kind !== "canvas" || before.kind === "canvas") return;
    // A touch screen shows the tapped card's finger-sized tools (globals.css, `data-tapped`): the
    // tap that opened what just closed (Escape, ×, Back) did its job, so the canvas comes back clear.
    const touch = window.matchMedia("(hover: none)").matches;
    if (touch) for (const el of document.querySelectorAll("[data-card][data-tapped]")) el.removeAttribute("data-tapped");
    const key = before.kind === "thread" ? `t:${before.thread}` : before.kind === "settings" && before.target !== "owner" ? before.target : null;
    const t = setTimeout(() => {
      const a = document.activeElement;
      // Focus left in a panel that is sliding out is lost too (it goes with the panel).
      if ((a && a !== document.body && a.isConnected && !a.closest("[data-panel][data-state=closing]")) || !lastInputWasKeyboard()) return;
      // A focused card shows its tools there too: focus goes back only where they don't cover the canvas.
      if (touch) return;
      const card = key ? root.current?.querySelector<HTMLElement>(`[data-card][data-key="${CSS.escape(key)}"]`) : null;
      const opener = lastCanvasFocus.current?.isConnected ? lastCanvasFocus.current : null;
      // Settings go back to what opened them (a card's ⋯, the sidebar's project menu), a conversation to its card.
      const back = before.kind === "settings" ? (opener ?? card) : (card ?? opener);
      back?.focus({ preventScroll: true });
    }, 0);
    return () => clearTimeout(t);
  }, [shown]);
  const startNew = useCallback((project?: string, opts?: { machine?: string; at?: { x: number; y: number } }) => {
    const r = root.current?.getBoundingClientRect();
    const at = opts?.at && r ? { x: Math.round(opts.at.x - r.left), y: Math.round(opts.at.y - r.top) } : undefined;
    setAgentsView({ kind: "new", ...(project ? { project } : {}), ...(opts?.machine ? { machine: opts.machine } : {}), ...(at ? { at } : {}) });
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

  // "Focus on this project only": the canvas, the sidebar and the counts scoped to one project,
  // remembered per view on this device, until Escape or the chip's ×.
  const viewId = views.current.id;
  const [focusedBy, setFocusedBy] = useState<Record<string, string>>(() => loadFocused(client.owner));
  const focused = focusedBy[viewId] ?? null;
  const focusProject = useCallback(
    (project: string | null) => {
      setFocusedBy((all) => setFocusedIn(client.owner, all, viewId, project));
      if (project) setAgentsView({ kind: "canvas" });
      // The tap that drilled in did its job: its card's finger-sized tools would cover the next view.
      for (const el of document.querySelectorAll("[data-card][data-tapped]")) el.removeAttribute("data-tapped");
    },
    [client.owner, viewId],
  );
  // Stable, for the memoised sidebar.
  const showAll = useCallback(() => focusProject(null), [focusProject]);
  const openAddMachine = useCallback(() => setAdding(true), []);
  // Settings' search points here ("sandbox", "add machine").
  useEffect(() => {
    window.addEventListener(ADD_MACHINE_EVENT, openAddMachine);
    return () => window.removeEventListener(ADD_MACHINE_EVENT, openAddMachine);
  }, [openAddMachine]);
  const focusedGone = focused !== null && data.projects.size > 0 && !data.projects.has(focused);
  useEffect(() => {
    if (focusedGone) focusProject(null);
  }, [focusedGone, focusProject]);
  const prefs = useOwnerSettings(services?.settings, client.owner, data.settings?.revision ?? 0);
  // The model picker's favourites and recent models follow the owner (a share's replay has no services).
  useModelPrefsSync(prefs, services ? prefs.save : null);
  // Many threads at once: the bulk actions, and "Next that needs me" (N, ⌘K) in every layout:
  // the longest-waiting thread's conversation, then the next, round to the first.
  const bulk = useBulk(client);
  // For surfaces off the page's bar (the Shelf): stable, so the context doesn't change with progress.
  const bulkNow = useRef(bulk);
  bulkNow.current = bulk;
  const runBulk = useCallback<NonNullable<AgentsCtx["bulk"]>>((spec, ids) => bulkNow.current.run(spec, ids), []);
  const ctx: AgentsCtx = useMemo(
    () => ({ client, owner: client.owner, demo, openThread, close, startNew, focus, focusProject, services, sandboxes, phone, prefs, bulk: runBulk }),
    [client, demo, openThread, close, startNew, focus, focusProject, services, sandboxes, phone, prefs, runBulk],
  );

  const nextNeedsMe = useCallback(() => {
    const d = client.data;
    const order = needsMe(d, rowsOf(d, { now: Date.now(), showArchived: false, filter: {}, focused }));
    const v = agentsSurface.get().view;
    const next = nextOf(order, v.kind === "thread" ? v.thread : null);
    if (next) openThread(next);
    else showInboxToast({ text: "Nothing needs you right now" });
  }, [client, focused, openThread]);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey || e.repeat || e.key !== "n") return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName) || el.closest(".xterm, [role=dialog]:not([data-surface]), [role=menu], [role=listbox]"))) return;
      e.preventDefault();
      nextNeedsMe();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [nextNeedsMe]);

  // ⌘N / ⌘⇧O (T3's `chat.new`): the launcher, except in a terminal. ⌘, (T3's "Settings…"): the
  // owner's settings beside the canvas; the header control does it on other pages, this on the
  // full page and the desktop app's Agents window.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && !e.altKey && !e.shiftKey && e.key === ",") {
        e.preventDefault();
        setAgentsView({ kind: "settings", target: "owner" });
        return;
      }
      if (!isNewAgentKey(e) || (e.target as HTMLElement | null)?.closest?.(".xterm")) return;
      e.preventDefault();
      startNew();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [startNew]);

  // A card's handle dropped on the canvas: the launcher there for that project or machine, or a
  // fork of that thread (T3's fork from the latest point), opened beside the canvas.
  const [launchError, setLaunchError] = useState<string | null>(null);
  useEffect(() => {
    if (!launchError) return;
    const t = setTimeout(() => setLaunchError(null), 5000);
    return () => clearTimeout(t);
  }, [launchError]);
  const onLaunch = useCallback(
    (source: string, at: { x: number; y: number }) => {
      const id = source.slice(2);
      if (source.startsWith("p:")) startNew(id, { at });
      else if (source.startsWith("m:")) startNew(undefined, { machine: id, at });
      else if (source.startsWith("t:")) {
        const m = client.data.threads.get(id)?.machine;
        if (!m) return;
        client
          .run<{ type: "created"; id: string }>(m, { type: "thread.fork", thread: id }, "created")
          .then((r) => openThread(r.id))
          .catch((e: Error) => setLaunchError(`Couldn't fork: ${e.message}`));
      }
    },
    [client, startNew, openThread],
  );

  // Escape: back to the canvas, and from the canvas out of the popup. Not while typing, except in
  // an empty composer (it has focus in a conversation, so Escape would never leave otherwise). A
  // popup opened straight into a conversation (a change page's link) goes back to that page.
  const enteredAt = useRef(onClose && shown.kind === "thread" ? shown.thread : null);
  if (enteredAt.current && !(shown.kind === "thread" && shown.thread === enteredAt.current)) enteredAt.current = null;
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape" || e.defaultPrevented) return;
      const el = e.target as HTMLElement | null;
      if (el?.closest(".xterm, [role=dialog]:not([data-surface]), [role=listbox], [role=menu]")) return;
      // The innermost thing open in a conversation first (a diff, a confirm, a form), even from a field.
      if (closeTopLayer()) {
        e.preventDefault();
        return;
      }
      // An empty search box (settings') isn't typing: its first Escape cleared it, the next closes.
      const emptySearch = el instanceof HTMLInputElement && el.type === "search" && el.value === "";
      const typing = el && !emptySearch && (el.isContentEditable || /^(INPUT|SELECT)$/.test(el.tagName) || (el.tagName === "TEXTAREA" && (el as HTMLTextAreaElement).value !== ""));
      if (typing) return;
      // The automations sheet goes up one level; from its list, it closes.
      if (shown.kind === "automations") (sheetBack.current ?? close)();
      else if (enteredAt.current) onClose?.();
      else if (shown.kind !== "canvas") close();
      else onClose?.();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shown, close, onClose]);

  // ⌘K: the command palette, while the agents are showing (T3's `commandPalette.toggle`). It
  // comes first, so the dashboard's own ⌘K ("Ask roll") stays for the page behind. A terminal
  // keeps Ctrl+K (kill to the end of the line).
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.shiftKey || e.altKey || e.key.toLowerCase() !== "k") return;
      if (!e.metaKey && (e.target as HTMLElement | null)?.closest?.(".xterm")) return;
      e.preventDefault();
      togglePalette();
    };
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("keydown", onKey, true);
      closePalette();
    };
  }, []);

  const onSandbox = useCallback(
    (m: { id: string }) => {
      setAdding(false);
      focus(`m:${m.id}`);
    },
    [focus],
  );

  const all = rolling(data);
  // Focused on one project: its counts only.
  const inFocus = (thread: string) => !focused || data.threads.get(thread)?.value.project === focused;
  const working = focused ? all.busy.filter((t) => t.project === focused) : all.busy;
  const requests = focused ? all.requests.filter((r) => inFocus(r.thread)) : all.requests;
  const toggleAutomations = useCallback(() => setAgentsView(shown.kind === "automations" ? { kind: "canvas" } : { kind: "automations" }), [shown.kind]);
  // Notices nobody saw (A12): the automations button says so and opens them.
  const unseen = useUnseenNotices(client, services.automations);
  const newNotices = unseen > 0 ? `${unseen} new notice${unseen === 1 ? "" : "s"}` : "";
  const openAutomations = useCallback(
    () => (unseen > 0 && shown.kind !== "automations" ? setAgentsView({ kind: "automations", list: "notices" }) : toggleAutomations()),
    [unseen, shown.kind, toggleAutomations],
  );
  // A (no modifiers, not typing, nothing open over the canvas): the automations sheet, like the ⚡ button.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || e.altKey || e.shiftKey || e.repeat || e.key.toLowerCase() !== "a") return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName) || el.closest(".xterm, [role=dialog]:not([data-surface]), [role=menu], [role=listbox]"))) return;
      e.preventDefault();
      openAutomations();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [openAutomations]);
  // Waiting on you isn't running: those show as the waiting count.
  const running = working.filter((t) => t.status !== "waiting").length;
  const archived = [...data.threads.values()].filter((r) => r.value.archived_at !== null).length;
  const display = views.current.display;
  const direction = directionOf(display, phone);
  const { setDisplay } = views;
  const setDirection = useCallback((d: Direction) => setDisplay(withDirection(display, phone, d)), [setDisplay, display, phone]);
  const host: PaletteHost = useMemo(
    () => ({
      addMachine: () => setAdding(true),
      showArchived,
      toggleArchived: () => setShowArchived((s) => !s),
      archived,
      fit: () => canvas.current?.fit(),
      views: views.views,
      view: views.current.id,
      selectView: views.select,
      direction,
      setDirection,
      display,
      setDisplay,
      deleteView: (id: string) => {
        views.select(ALL);
        views.update((d) => deleteView(d, id));
      },
      focused,
      focusProject,
      scale: { display, setDisplay, nextNeedsMe, bulk: bulk.run },
    }),
    [showArchived, archived, views, direction, setDirection, display, setDisplay, focused, focusProject, nextNeedsMe, bulk.run],
  );
  const hint = useSyncExternalStore(noSubscribe, () => newAgentHint(navigator.platform), () => "⌘N");
  const end = (
    <>
      <button
        onClick={() => startNew()}
        className="ml-1 inline-flex h-7 items-center gap-1.5 rounded-md bg-fg pr-2.5 pl-2 text-[12.5px] font-medium tracking-[-0.01em] text-bg shadow-card inset-shadow-[0_1px_0_rgb(255_255_255/0.25)] transition-[opacity,scale] duration-150 hover:opacity-90 active:scale-[0.97]"
        title={`New agent (${hint})`}
      >
        <Plus className="size-3.5" strokeWidth={2} />
        <span className="max-sm:sr-only">New agent</span>
      </button>
      {onClose && (
        <>
          <span className="mx-1 h-4 w-px bg-line-2" aria-hidden />
          <IconButton label="Close (Esc)" onClick={onClose}>
            <X className="size-4" strokeWidth={1.75} />
          </IconButton>
        </>
      )}
    </>
  );

  return (
    <AgentsLayer><AgentsContext.Provider value={ctx}>
      <div ref={attachRoot} className="relative flex h-full min-h-0 flex-col bg-bg text-fg">
        {/* The full window drives the logo itself (in the popup, the header already does). */}
        {chrome === "window" && <MarkDriver client={client} />}
        <div className="flex min-h-0 flex-1 flex-col">
          <header className="chrome-in relative z-10 flex h-12 shrink-0 items-center gap-3 pr-2 pl-2 max-sm:gap-2 max-sm:pr-1 max-sm:pl-1" data-drag-region>
            <span className="flex shrink-0 items-center gap-2.5">
              <SidebarToggle open={sidebar} onToggle={() => setSidebar(!sidebar)} />
              {chrome === "window" ? <WindowHome /> : <HeaderMark />}
              <h2 className="text-[14px] font-[590] tracking-[-0.02em]">Agents</h2>
              {admin && (
                <span data-nodrag>
                  <OwnerSwitcher owner={client.owner} className="w-36 max-sm:w-28" />
                </span>
              )}
            </span>
            <span className="h-4 w-px shrink-0 bg-line-2 max-sm:hidden" aria-hidden />
            <span className="flex min-w-0 items-center gap-2 text-[12.5px] text-muted">
              {(() => {
                // "All idle" only when it's true: not beside something waiting, not with every machine offline.
                const anyOnline = [...data.machines.values()].some((r) => reachable(r.value));
                const [dot, label] =
                  snap.conn === "error"
                    ? ["bg-danger", snap.error ?? "Can't reach roll"]
                    : snap.conn !== "live"
                      ? ["bg-subtle", "Reconnecting…"]
                      : running
                        ? ["pulse-dot bg-accent", `${running} running`]
                        : data.machines.size === 0
                          ? ["bg-subtle", "No machines yet"]
                          : !anyOnline
                            ? ["bg-subtle", data.machines.size === 1 ? "Machine offline" : "Machines offline"]
                            : requests.length > 0
                              ? ["bg-ok", "Nothing running"]
                              : ["bg-ok", "All idle"];
                return (
                  <>
                    <span className={`size-1.5 shrink-0 rounded-full ${dot}`} aria-hidden />
                    <span className="truncate">{label}</span>
                  </>
                );
              })()}
              {requests.length > 0 && (
                <button
                  onClick={() => openThread(requests[0].thread)}
                  aria-label={`${requests.length} waiting for you`}
                  className="inline-flex h-[22px] shrink-0 items-center gap-1.5 rounded-full border border-accent/30 bg-accent/10 pr-2 pl-1 text-[11.5px] max-sm:pr-1 font-medium text-accent transition-colors duration-150 hover:bg-accent/20"
                  data-nodrag
                >
                  <span className="grid h-4 min-w-4 place-items-center rounded-full bg-accent px-1 font-mono text-[10px] font-semibold text-white tabular-nums">{requests.length}</span>
                  <span className="max-sm:hidden">waiting for you</span>
                </button>
              )}
              {demo && <span className="shrink-0 rounded-[5px] max-sm:hidden border border-line-2 px-1.5 text-[10px] leading-[18px] font-medium tracking-[0.06em] text-subtle uppercase">Demo</span>}
            </span>
            <span className="ml-auto flex shrink-0 items-center gap-0.5 text-muted" data-nodrag>
              <button
                onClick={() => openPalette()}
                className="mr-2 flex h-7 w-60 items-center gap-2 rounded-md border border-line bg-fg/[0.03] pr-1 pl-2.5 text-[12.5px] text-subtle inset-shadow-highlight transition-colors duration-150 hover:border-line-2 hover:bg-fg/[0.05] hover:text-muted max-lg:w-44 max-sm:w-7 max-sm:justify-center max-sm:border-0 max-sm:bg-transparent max-sm:px-0"
                title="Search, run an action, or say what to do (⌘K)"
                aria-label="Command palette"
              >
                <Search className="size-3.5 shrink-0" strokeWidth={1.75} />
                <span className="min-w-0 flex-1 truncate text-left max-sm:hidden">Search or ask agents…</span>
                <kbd className="rounded-[4px] border border-line-2 bg-fg/[0.04] px-1 font-sans text-[10.5px] leading-[16px] text-subtle max-sm:hidden">⌘K</kbd>
              </button>
              {/* On a phone these are under More; the bar keeps its room for what is live. */}
              <span className="flex max-sm:hidden">
                <IconButton label={shown.kind === "automations" ? "Close automations (A)" : newNotices ? `Automations (A): ${newNotices}` : "Automations (A)"} active={shown.kind === "automations"} onClick={openAutomations}>
                  <span className="relative grid place-items-center">
                    <Zap className="size-4" strokeWidth={1.75} />
                    {unseen > 0 && <span className="absolute -top-0.5 -right-0.5 size-1.5 rounded-full bg-accent" aria-hidden />}
                  </span>
                </IconButton>
                <IconButton label="Add a machine" onClick={() => setAdding(true)}>
                  <Server className="size-4" strokeWidth={1.75} />
                </IconButton>
                <IconButton label="Agents settings (⌘,)" onClick={() => setAgentsView({ kind: "settings", target: "owner" })}>
                  <Settings className="size-4" strokeWidth={1.75} />
                </IconButton>
              </span>
              <Menu
                label="More"
                trigger={<MoreHorizontal className="size-4" strokeWidth={1.75} />}
                items={[
                  phone && { label: shown.kind === "automations" ? "Close automations" : newNotices ? `Automations · ${newNotices}` : "Automations", icon: <Zap />, onSelect: openAutomations },
                  phone && { label: "Add a machine", icon: <Server />, onSelect: () => setAdding(true) },
                  phone && { label: "Agents settings", icon: <Settings />, onSelect: () => setAgentsView({ kind: "settings", target: "owner" }) },
                  archived > 0 && { label: `${showArchived ? "Hide" : "Show"} archived (${archived})`, icon: <Archive />, onSelect: () => setShowArchived((s) => !s) },
                  // In the agents' own settings (it was a page outside the agents, the general dashboard's).
                  !demo && { label: "Shared links", icon: <Share2 />, onSelect: () => openSettings("owner", "Shared links") },
                ]}
              />
              {end}
            </span>
          </header>
          <div className="relative flex min-h-0 flex-1">
            <ThreadsSidebar data={data} open={sidebar} setOpen={setSidebar} phone={phone} openThread={shown.kind === "thread" ? shown.thread : null} only={focused} onShowAll={showAll} onAddMachine={openAddMachine} />
            {/* The canvas and the right sheet beside it; the sheet slides in past its right edge. */}
            <div className="relative flex min-h-0 min-w-0 flex-1 overflow-x-clip">
            {/* The working area: an inset panel one step up from the chrome (Linear's inverted L). It
                folds to nothing (still mounted) under a maximized builder, as the sheet grows over it. */}
            <div
              data-waiting={(phone && shown.kind === "canvas" && layoutOf(display) === "canvas" && requests.length > 0) || undefined}
              data-folded={wide || undefined}
              inert={wide}
              aria-hidden={wide || undefined}
              className={`relative min-h-0 min-w-0 flex-1 overflow-hidden bg-canvas ${phone ? "" : `mb-2 rounded-xl border-line transition-[margin,border-width,opacity] duration-[260ms] ease-(--ease-drawer) motion-reduce:transition-none ${wide ? "mx-0 border-0 opacity-0" : "mx-2 border"}`}`}
            >
              {data.machines.size === 0 && snap.conn !== "connecting" ? (
                <Empty demo={demo} onCreated={onSandbox} />
              ) : (
                <>
                  {/* Live beside the automations sheet; hidden (still mounted) under a maximized builder. */}
                  <div className="contents">
                    <CanvasView
                      canvasRef={canvas}
                      data={data}
                      showArchived={showArchived}
                      views={views}
                      direction={direction}
                      onDirection={setDirection}
                      focused={focused}
                      onFocus={focusProject}
                      pendingFocus={pendingFocus}
                      visible={shown.kind !== "new" && !wide && !(phone && shown.kind === "automations")}
                      selected={selected}
                      highlighted={shown.kind === "automations" ? highlighted : undefined}
                      phone={phone}
                      onLaunch={onLaunch}
                      keepClear={dock}
                      bulk={bulk}
                    />
                  </div>
                </>
              )}
              <AnimatePresence initial={false}>{launchError && <LaunchError key={launchError} text={launchError} />}</AnimatePresence>
              {/* Beside an open conversation too (the canvas narrows; the draft stays); on a phone, under what's waiting. */}
              {/* A phone's "Waiting for you" rides over the canvas; a list or board has them under Needs you (it covered 40% of them). */}
              {data.machines.size > 0 && <PromptBar ref={dock} above={phone && shown.kind === "canvas" && layoutOf(display) === "canvas" && requests.length > 0 && <PhoneWaiting client={client} />} />}
            </div>
            <RightDock
              phone={phone}
              wide={wide}
              pane={
                shown.kind === "thread" && threadOpen
                  ? {
                      key: `thread:${shown.thread}`,
                      label: "Conversation",
                      node: (
                        <Conversation
                          thread={threadOpen.value}
                          machine={data.machines.get(threadOpen.machine)?.value}
                          end={!phone && <PinConversation views={views} thread={threadOpen.value.id} />}
                          start={
                            <IconButton label="Close the conversation (Esc)" onClick={close}>
                              {phone ? <ArrowLeft className="size-4" /> : <PanelRightClose className="size-4" />}
                            </IconButton>
                          }
                        />
                      ),
                    }
                  : shown.kind === "settings"
                    ? { key: `settings:${shown.target}`, label: "Settings", node: <SettingsPanel target={shown.target} onClose={close} /> }
                    : shown.kind === "automations"
                      ? {
                          key: "automations",
                          label: "Automations",
                          node: <AutomationsSheet view={shown} phone={phone} maximized={maximized} setMaximized={setMaximized} backRef={sheetBack} onHighlight={setHighlighted} />,
                        }
                      : null
              }
            />
            </div>
          </div>
        </div>
        <Notices />
        <SelectionHost data={data} />
        <ProjectDialogs />
        {addMachine.value && <AddMachineDialog key={addMachine.key} open={addMachine.open} onExited={addMachine.onExited} onClose={() => setAdding(false)} onCreated={onSandbox} />}
        {launcher.value && (
          <Launcher
            key={launcher.key}
            open={launcher.open}
            onExited={launcher.onExited}
            container={rootEl}
            project={launcher.value.project}
            machine={launcher.value.machine}
            at={launcher.value.at}
            onClose={close}
          />
        )}
        <CommandPalette host={host} />
      </div>
    </AgentsContext.Provider></AgentsLayer>
  );
}

const MAXIMIZED = "roll:agents:automations-maximized";

/** The automations builder filling the working area (T3's maximized right panel), per device. */
function useMaximized(): [boolean, (on: boolean) => void] {
  const [on, setOn] = useState(false);
  useEffect(() => {
    try {
      setOn(localStorage.getItem(MAXIMIZED) === "1");
    } catch {
      // Not remembered.
    }
  }, []);
  const set = useCallback((next: boolean) => {
    setOn(next);
    try {
      localStorage.setItem(MAXIMIZED, next ? "1" : "0");
    } catch {
      // Not remembered.
    }
  }, []);
  return [on, set];
}

function MarkDriver({ client }: { client: AgentsClient }) {
  useMarkDriver(useAgentsSnapshot(client));
  return null;
}

/**
 * Why a drop on the canvas didn't launch: rises in, and after 5 s sinks back out the way it came
 * (it vanished), no longer announced or in the way as it goes.
 */
function LaunchError({ text }: { text: string }) {
  const reduced = useReducedMotion();
  const present = useIsPresent();
  return (
    <motion.p
      role={present ? "alert" : undefined}
      aria-hidden={!present || undefined}
      inert={!present}
      initial={{ opacity: 0, y: 6 }}
      animate={{ opacity: 1, y: 0, transition: reduced ? { duration: 0 } : POP_IN }}
      exit={{ opacity: 0, y: 6, transition: reduced ? { duration: 0 } : POP_OUT }}
      className="absolute bottom-4 left-4 z-10 rounded-lg border border-danger/40 bg-panel px-3 py-2 text-[12px] text-danger"
    >
      {text}
    </motion.p>
  );
}

/** The popup's logo: the Horizon mark, following the shared state. */
function HeaderMark() {
  const state = useMarkStateValue();
  return (
    <span className="flex items-center" aria-hidden>
      <HorizonMark state={state} size={22} label="" />
    </span>
  );
}

/** The full window's way back to the rest of roll. */
function WindowHome() {
  const { state } = useMarkState();
  return (
    <Link href="/dashboard" className="flex items-center" aria-label="roll" title="Open roll" data-nodrag>
      <HorizonMark state={state} size={22} label="roll" />
    </Link>
  );
}

/**
 * Memoised: the surface renders for things beside the canvas too (⌘B, a panel, a toast), and each
 * render of this one renders every card again (~200 at 200 threads).
 */
const CanvasView = memo(CanvasViewBody);

/** What the canvas's cards read (`CanvasCard`), from the canvas view. */
interface CardInputs {
  data: AgentsData;
  facts: (t: Thread) => ThreadFacts;
  view: CanvasViews["current"];
  toggle: (kind: PinRec["kind"], id: string) => void;
  resize: CanvasViews["resize"];
  folded: CanvasFolds["folded"];
}

/** The newest `CardInputs`, and who to tell when they change. */
class CardStore {
  private subs = new Set<() => void>();
  constructor(public current: CardInputs) {}
  subscribe = (fn: () => void) => {
    this.subs.add(fn);
    return () => void this.subs.delete(fn);
  };
  notify() {
    for (const fn of this.subs) fn();
  }
}

/**
 * Everything a card shows, from its inputs: it renders again only when one of these changed.
 * Cards that read the whole store while rendering (folds, requests, pins: a few) take `data`.
 */
function cardDeps(i: CardInputs, n: GraphNode): readonly unknown[] {
  const { data } = i;
  const machine = data.machines.get(n.machine)?.value;
  switch (n.kind) {
    case "machine":
      return [data.machines.get(n.id)?.value, data.projects];
    case "project":
      // Its stats count its threads and their runs; its providers are its machine's.
      return [data.projects.get(n.id)?.value, machine, data.threads, data.runs];
    case "thread": {
      const t = data.threads.get(n.id)?.value;
      const f = t && i.facts(t);
      return [t, machine, f?.project, f?.lastRun, f?.pending, f?.terminals, f?.orchestrated, i.view, i.toggle];
    }
    case "subagent":
      return [data.nodes, data.threads, data.projects];
    case "terminal":
      return [data.terminals.get(n.id)?.value, machine, i.view, i.toggle];
    case "group":
      return [n.children.filter((c) => c.startsWith("t:")).length, i.folded.get(n.key)];
    case "fold":
    case "request":
    case "pin":
      return [data, i.view, i.toggle, i.resize];
  }
}

const sameDeps = (a: readonly unknown[], b: readonly unknown[]) => a.length === b.length && a.every((x, k) => Object.is(x, b[k]));

/** One card on the canvas, rendering from the store when its own inputs change (`cardDeps`). */
const CanvasCard = memo(function CanvasCard({ node: n, store }: { node: GraphNode; store: CardStore }) {
  const last = useRef<readonly unknown[] | null>(null);
  const snapshot = () => {
    const next = cardDeps(store.current, n);
    if (last.current && sameDeps(last.current, next)) return last.current;
    last.current = next;
    return next;
  };
  useSyncExternalStore(store.subscribe, snapshot, snapshot);
  const { data, facts, view, toggle, resize, folded } = store.current;
  const machine = data.machines.get(n.machine)?.value;
  switch (n.kind) {
    case "machine": {
      const m = data.machines.get(n.id)?.value;
      return m ? <MachineNode machine={m} /> : null;
    }
    case "project": {
      const p = data.projects.get(n.id)?.value;
      return p ? <ProjectNode project={p} machine={machine} /> : null;
    }
    case "thread": {
      const t = data.threads.get(n.id)?.value;
      if (!t) return null;
      return (
        <PinTools
          view={view}
          onToggle={toggle}
          items={[
            { kind: "conversation", id: n.id, label: "the conversation" },
            { kind: "diff", id: n.id, label: "the diff" },
          ]}
        >
          <ThreadCard thread={t} machine={machine} {...facts(t)} />
        </PinTools>
      );
    }
    case "subagent": {
      const node = [...data.nodes.values()].map((m) => m.get(n.id)).find(Boolean)?.value;
      return node ? <SubagentNode node={node} /> : null;
    }
    case "request": {
      const r = data.requests.get(n.id)?.value;
      return r ? <RequestNode request={r} /> : null;
    }
    case "terminal": {
      const t = data.terminals.get(n.id)?.value;
      if (!t) return null;
      return (
        <PinTools view={view} onToggle={toggle} items={[{ kind: "terminal", id: n.id, label: "the terminal" }]} extra={<CloseTerminal terminal={t} machine={machine} />}>
          <TerminalNode terminal={t} machine={machine} />
        </PinTools>
      );
    }
    case "fold":
      return <FoldedThreads container={n.id} />;
    case "group": {
      const fold = folded.get(n.key);
      return <StatusHeader group={n.id as StatusGroup} count={n.children.filter((c) => c.startsWith("t:")).length + (fold?.expanded === false ? fold.threads.length : 0)} />;
    }
    case "pin": {
      const p = pinOf(view, n.key);
      if (!p) return null;
      const props = { pin: p, onUnpin: () => toggle(p.kind, p.id), onResize: (w: number, h: number) => resize(p, w, h) };
      if (p.kind === "terminal") {
        const t = data.terminals.get(p.id)?.value;
        return t ? <PinnedTerminal {...props} terminal={t} machine={machine} /> : null;
      }
      const t = data.threads.get(p.id)?.value;
      if (!t) return null;
      return p.kind === "conversation" ? <PinnedConversation {...props} thread={t} machine={machine} /> : <PinnedDiff {...props} thread={t} machine={machine} />;
    }
  }
});

function CanvasViewBody({
  canvasRef,
  data,
  showArchived,
  views,
  direction,
  onDirection,
  focused,
  onFocus,
  pendingFocus,
  visible,
  selected,
  highlighted,
  phone,
  onLaunch,
  keepClear,
  bulk,
}: {
  canvasRef: React.RefObject<CanvasHandle | null>;
  data: AgentsClient["data"];
  showArchived: boolean;
  views: CanvasViews;
  direction: Direction;
  onDirection: (d: Direction) => void;
  /** The project filling the canvas, if one does. */
  focused: string | null;
  onFocus: (project: string | null) => void;
  pendingFocus: React.RefObject<string | null>;
  visible: boolean;
  selected?: string;
  /** More cards to mark as open beside the canvas (a run's threads in the automations sheet). */
  highlighted?: readonly string[];
  phone: boolean;
  onLaunch?: (source: string, at: { x: number; y: number }) => void;
  keepClear?: React.RefObject<HTMLElement | null>;
  bulk: Bulk;
}) {
  const { client, owner, readOnly } = useAgents();
  const view = views.current;
  // Deferred: the field updates at once and the canvas follows in an interruptible render (each
  // key re-laid out every card: ~150 ms a key at 200 threads).
  const filter = useDeferredValue(views.filter);
  // The same threads as a list or a board (the view's layout). The canvas isn't mounted under them:
  // kept hidden it rendered every card again on each record of the stream (at 1000 threads that
  // was most of every frame); its frame is remembered per view, so it comes back where it was.
  const layout = layoutOf(view.display);
  const canvasShown = layout === "canvas";
  const settle = useCallback((ids: readonly string[]) => bulk.run({ kind: "settle" }, ids), [bulk]);
  const collection = useCollection({
    data: layout === "canvas" ? EMPTY_DATA : data,
    filter,
    display: view.display,
    setDisplay: views.setDisplay,
    showArchived,
    focused,
    open: selected?.startsWith("t:") ? selected.slice(2) : null,
    phone,
    storageKey: viewStorageKey(owner, view.id),
    settle,
  });
  // ⌘A picks every thread the view shows, whatever its layout (the list and the board take it
  // themselves while they have focus): on the canvas, or just after picking a view's tab, it
  // selected the page's text instead.
  useEffect(() => {
    if (!visible || readOnly) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || !(e.metaKey || e.ctrlKey) || e.altKey || e.shiftKey || e.key.toLowerCase() !== "a") return;
      const el = e.target as HTMLElement | null;
      if (el && (el.isContentEditable || /^(INPUT|SELECT|TEXTAREA)$/.test(el.tagName) || el.closest(".xterm, [data-panel], [role=dialog]:not([data-surface]), [role=menu], [role=listbox]"))) return;
      e.preventDefault();
      selection.toggleAll(rowsOf(data, { now: Date.now(), showArchived, filter, focused }).map((r) => r.id));
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [visible, readOnly, data, showArchived, filter, focused]);
  // A pinned conversation answers its thread's requests itself.
  const expanded = useMemo(() => pinnedConversations(view), [view]);
  // The open conversation's terminals are in its drawer: not a second time on the canvas.
  const docked = selected?.startsWith("t:") ? selected.slice(2) : null;
  // Only the canvas needs the graph (the list and the board are rows).
  const graphData = canvasShown ? data : EMPTY_DATA;
  const whole = useMemo(() => deriveGraph(graphData, { showArchived, expanded, docked }), [graphData, showArchived, expanded, docked]);
  const display = view.display;
  const viewed = useMemo(() => {
    const g = applyView(whole, data, { filter, pins: view.pins }, Date.now());
    return applyDisplay(g, display, {
      // Idle: nothing going on and nothing unseen (a finished run you haven't opened stays).
      idle: (id) => {
        const t = data.threads.get(id)?.value;
        if (!t) return false;
        const runs = runsOf(data, id);
        const last = t.active_run ? data.runs.get(t.active_run)?.value : runs[runs.length - 1];
        return !isBusy(t.status) && t.status !== "error" && t.pending_requests === 0 && !threadDone(t, last);
      },
      name: (k) => data.projects.get(k.slice(2))?.value.name ?? k,
    });
  }, [whole, data, filter, view.pins, display]);
  const narrowed = useMemo(() => (focused ? focusOn(viewed, `p:${focused}`) : viewed), [viewed, focused]);
  // The canvas at scale (docs/code/agents.md, "Canvas"): the overview shows each project's work
  // that matters now and folds its quiet threads into one card; drilled into a project (its space,
  // `focused`), its threads sit under status headers and only long Done and Quiet groups fold.
  const [expandedFolds, setExpandedFolds] = useState<ReadonlySet<string>>(() => new Set());
  // Threads asked for (a deep link, ⌘K) keep their card while they're shown.
  const [asked, setAsked] = useState<ReadonlySet<string>>(() => new Set());
  const folds = useMemo(() => {
    const newest = newestRuns(data);
    const highlight = new Set(highlighted ?? []);
    const keep = (id: string) => {
      const t = data.threads.get(id)?.value;
      return !!t && (isBusy(t.status) || t.status === "error" || t.pending_requests > 0 || id === docked || asked.has(id) || highlight.has(`t:${id}`));
    };
    const space = focused ? `p:${focused}` : null;
    const grouped = space
      ? groupByStatus(narrowed, space, (id) => {
          const t = data.threads.get(id)!.value;
          if (t.pending_requests > 0) return "needs";
          if (isBusy(t.status)) return "running";
          if (t.status === "error") return "failed";
          return threadDone(t, newest.get(id)) ? "done" : "quiet";
        })
      : narrowed;
    return foldQuiet(grouped, {
      shown: (n) => (space ? (n.kind === "group" && (n.id === "done" || n.id === "quiet") ? SHOWN_IN_GROUP : null) : n.kind === "project" ? SHOWN_THREADS : null),
      keep,
      activity: (id) => {
        const t = data.threads.get(id)?.value;
        const r = newest.get(id);
        return t ? Math.max(t.created_at, r?.requested_at ?? 0, r?.completed_at ?? 0) : 0;
      },
      expanded: expandedFolds,
    });
  }, [narrowed, data, docked, asked, highlighted, expandedFolds, focused]);
  const foldsCtx: CanvasFolds = useMemo(
    () => ({
      folded: folds.folded,
      toggle: (container) =>
        setExpandedFolds((prev) => {
          const next = new Set(prev);
          if (!next.delete(container)) next.add(container);
          return next;
        }),
      drill: (project) => onFocus(project),
      drilled: focused,
    }),
    [folds.folded, onFocus, focused],
  );
  const folded = folds.graph;
  // What Fit frames when everything is too much to read: work going on, waiting on you or failed.
  // Read when used, so the canvas and the cards' tools keep one callback while records stream.
  const latest = useRef(data);
  useLayoutEffect(() => {
    latest.current = data;
  });
  const matters = useCallback((key: string) => {
    if (!key.startsWith("t:")) return false;
    const t = latest.current.threads.get(key.slice(2))?.value;
    return !!t && (isBusy(t.status) || t.status === "error" || t.pending_requests > 0);
  }, []);
  // Projects with nothing on the canvas fold into their machine's "3 more projects", unless pinned
  // onto the view, focused, or named by its filter.
  const collapsed = useMemo(() => {
    if (display.projects === "all") return { graph: folded, unused: new Map<string, readonly string[]>() };
    const keep = new Set([...(display.keep ?? []), ...(filter.projects ?? []), ...(focused ? [focused] : [])]);
    return collapseUnused(folded, (p) => keep.has(p));
  }, [folded, display.projects, display.keep, filter.projects, focused]);
  // Kept while its shape is (`sameGraph`): the canvas renders again only when that changes.
  const shaped = useRef<Graph | null>(null);
  const graph = useMemo(() => (shaped.current = sameGraph(shaped.current, collapsed.graph)), [collapsed.graph]);
  const { setDisplay } = views;
  const projects: CanvasProjects = useMemo(
    () => ({
      unused: collapsed.unused,
      kept: (p) => !!display.keep?.includes(p),
      keep: readOnly ? undefined : (p, on) => setDisplay(keepProject(display, p, on)),
    }),
    [collapsed.unused, display, readOnly, setDisplay],
  );
  // F on a card focuses its project (again: everything); Escape lets go first.
  useEffect(() => {
    if (!visible) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey || e.ctrlKey || e.altKey || e.defaultPrevented) return;
      const el = e.target as HTMLElement | null;
      // Only on the canvas: the side panel, fields and popups keep their keys.
      if (el?.closest?.("input, textarea, [contenteditable], .xterm, [data-panel], [role=dialog]:not([data-surface]), [role=menu], [role=listbox]")) return;
      if (e.key === "Escape" && focused) {
        e.preventDefault();
        e.stopPropagation();
        onFocus(null);
        return;
      }
      if (e.key !== "f") return;
      const key = el?.closest<HTMLElement>("[data-card]")?.dataset.key;
      const project = key ? projectOf(viewed, key) : null;
      if (!project && !focused) return;
      e.preventDefault();
      onFocus(focused ? null : project!.slice(2));
      // Keep the card focused once the canvas has redrawn.
      if (key) setTimeout(() => (document.querySelector<HTMLElement>(`[data-card][data-key="${CSS.escape(key)}"] [data-drag][tabindex='0']`) ?? document.querySelector<HTMLElement>(`[data-card][data-key="${CSS.escape(key)}"]`))?.focus({ preventScroll: true }), 0);
    };
    // Capture: before Escape closes the popup.
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [visible, focused, viewed, onFocus]);
  // Enter on a machine or project card opens its settings beside the canvas.
  const onOpen = useCallback((key: string) => {
    if (!key.startsWith("m:") && !key.startsWith("p:")) return false;
    setAgentsView({ kind: "settings", target: key });
    return true;
  }, []);
  // Pinned conversations and diffs need their thread's detail.
  const followed = view.pins
    .filter((p) => p.kind !== "terminal")
    .map((p) => p.id)
    .sort()
    .join();
  useEffect(() => {
    client.follow("pins", followed ? followed.split(",") : []);
  }, [client, followed]);
  useEffect(() => () => client.unfollow("pins"), [client]);
  // A card just pinned comes into view once it's laid out (another view's pins aren't new).
  const pinsSeen = useRef<{ view: string; keys: Set<string> } | null>(null);
  useEffect(() => {
    const keys = new Set(view.pins.map(pinKey));
    const before = pinsSeen.current;
    pinsSeen.current = { view: view.id, keys };
    if (!before || before.view !== view.id) return;
    const added = [...keys].filter((k) => !before.keys.has(k));
    if (!added.length) return;
    const t = setTimeout(() => canvasRef.current?.reveal(added[added.length - 1]), 320);
    return () => clearTimeout(t);
  }, [view.id, view.pins, canvasRef]);
  // Show a node asked for before the canvas had it (a deep link, a new sandbox).
  useEffect(() => {
    const k = pendingFocus.current;
    if (!k || !visible) return;
    if (!graph.nodes.has(k)) {
      // A project without a card: its machine, which lists it.
      const p = k.startsWith("p:") ? data.projects.get(k.slice(2)) : undefined;
      if (p && collapsed.unused.get(p.machine)?.includes(p.value.id)) {
        pendingFocus.current = null;
        for (const ms of [60, 420]) setTimeout(() => canvasRef.current?.focus(`m:${p.machine}`), ms);
        return;
      }
      // A quiet thread folded into its project's card: it keeps a card of its own while asked for.
      if (k.startsWith("t:") && !asked.has(k.slice(2)) && [...folds.folded.values()].some((f) => f.threads.includes(k.slice(2)))) {
        setAsked((prev) => new Set(prev).add(k.slice(2)));
        return;
      }
      // Not in this view: All has everything.
      if (view.id !== ALL && whole.nodes.has(k)) views.select(ALL);
      return;
    }
    pendingFocus.current = null;
    for (const ms of [60, 420]) setTimeout(() => canvasRef.current?.focus(k), ms);
  });
  const { togglePin } = views;
  const toggle = useCallback(
    (kind: PinRec["kind"], id: string) => togglePin(kind, id, (p: PinRec) => (p.kind === "terminal" ? latest.current.terminals.has(p.id) : latest.current.threads.has(p.id))),
    [togglePin],
  );
  const facts = useThreadFacts(data);
  // What the cards read. Each card picks its own inputs from it (`CanvasCard`), so a streamed
  // record renders its own card, and the canvas renders again only when the graph's shape changed:
  // rendering every card's slot on each batch was ~1,700 components (tasks of 20–40 ms at 1000 threads).
  const { resize } = views;
  const inputs = useMemo<CardInputs>(() => ({ data, facts, view, toggle, resize, folded: folds.folded }), [data, facts, view, toggle, resize, folds.folded]);
  const [cardStore] = useState(() => new CardStore(inputs));
  // Set while rendering, so cards rendering in this pass read it; the others hear of it below.
  cardStore.current = inputs;
  useLayoutEffect(() => cardStore.notify(), [cardStore, inputs]);
  const render = useCallback((n: GraphNode) => <CanvasCard node={n} store={cardStore} />, [cardStore]);
  const sizeOf = useCallback(
    (n: GraphNode): Size => {
      if (n.kind !== "pin") return defaultSize(n);
      const p = pinOf(view, n.key);
      return p ? { w: p.w, h: p.h } : { w: 520, h: 600 };
    },
    [view],
  );
  const empty = graph.nodes.size === 0 && whole.nodes.size > 0;
  // What this layout shows: the bulk bar says how many selected threads it doesn't.
  const shownIds = useMemo(() => new Set(layout === "canvas" ? [...graph.nodes.values()].filter((n) => n.kind === "thread").map((n) => n.id) : collection.rows.map((r) => r.id)), [layout, graph, collection.rows]);
  return (
    <div className="flex h-full min-h-0 flex-col">
      <ViewsBar views={views} data={data} active={visible} phone={phone} reveal={(k) => canvasRef.current?.reveal(k)} focused={focused ? { id: focused, name: data.projects.get(focused)?.value.name ?? "project", clear: () => onFocus(null) } : undefined} />
      <BulkBar data={data} bulk={bulk} phone={phone} visible={shownIds} />
      <div className="relative min-h-0 flex-1">
        {layout !== "canvas" && (
          <div key={layout} className="fade-in absolute inset-0">
            <ThreadCollection layout={layout} collection={collection} />
          </div>
        )}
        {canvasShown && (
        <CanvasProjectsProvider value={projects}>
          <CanvasFoldsProvider value={foldsCtx}>
          <AgentsCanvas
            ref={canvasRef}
            graph={graph}
            storageKey={viewStorageKey(owner, view.id)}
            order={view.order}
            onOrder={views.setOrder}
            fitKey={`${JSON.stringify(filter)}|${focused ?? ""}|${display.group ?? ""}|${display.hide?.join() ?? ""}|${display.density ?? ""}|${display.projects ?? ""}`}
            direction={direction}
            onDirection={onDirection}
            density={display.density}
            onOpen={onOpen}
            render={render}
            defaultSize={sizeOf}
            active={visible}
            selected={selected}
            highlighted={highlighted}
            matters={matters}
            onLaunch={onLaunch}
            keepClear={keepClear}
            overlay={empty && <EmptyView name={view.id === ALL ? null : view.name} waitingOnly={filter.status?.length === 1 && filter.status[0] === "waiting"} onAll={() => (view.id === ALL ? views.setFilter({}) : views.select(ALL))} />}
          />
          </CanvasFoldsProvider>
        </CanvasProjectsProvider>
        )}
      </div>
    </div>
  );
}

const EMPTY_DATA = emptyData();

/** A view that picks nothing right now: say so calmly, and the way back to everything. */
function EmptyView({ name, waitingOnly, onAll }: { name: string | null; waitingOnly: boolean; onAll: () => void }) {
  return (
    <div className="fade-in pointer-events-none absolute inset-0 grid place-items-center px-6 text-center">
      <div className="pointer-events-auto space-y-2">
        <p className="text-[13.5px] font-medium tracking-[-0.01em]">{waitingOnly ? "Nothing is waiting on you" : name ? `Nothing in ${name} right now` : "No threads match the filter"}</p>
        <p className="text-[12.5px] text-subtle">{waitingOnly ? "Requests show up here the moment an agent asks." : "Threads show up here as soon as they match."}</p>
        <button onClick={onAll} className="text-[12.5px] text-accent hover:underline">
          {name ? "Show all" : "Clear the filter"}
        </button>
      </div>
    </div>
  );
}

/**
 * How long a conversation asked for may take to arrive: a new thread's or a fork's record comes on
 * the stream after the command's reply, and opening it then mustn't count as "gone".
 */
export const ARRIVAL_GRACE_MS = 5000;

/**
 * Whether the open conversation `id` is gone: it was here and isn't any more (deleted), or it
 * never arrived within [`ARRIVAL_GRACE_MS`] (a link to one that isn't this owner's, or long gone).
 */
function useGone(id: string | null, present: boolean, live: boolean): boolean {
  const [seen, setSeen] = useState<string | null>(null);
  const [late, setLate] = useState<string | null>(null);
  useEffect(() => {
    if (id && present) setSeen(id);
  }, [id, present]);
  useEffect(() => {
    if (!id || present || !live) return;
    const t = setTimeout(() => setLate(id), ARRIVAL_GRACE_MS);
    return () => clearTimeout(t);
  }, [id, present, live]);
  return !!id && !present && live && (seen === id || late === id);
}

/** The side panel's way to put its conversation on the canvas view that's showing. */
function PinConversation({ views, thread }: { views: CanvasViews; thread: string }) {
  const pinned = isPinned(views.current, "conversation", thread);
  const where = views.current.id === ALL ? "the canvas" : views.current.name;
  return (
    <IconButton label={pinned ? `Unpin from ${where}` : `Pin to ${where}`} active={pinned} onClick={() => views.togglePin("conversation", thread)}>
      {pinned ? <PinOff className="size-4" /> : <Pin className="size-4" />}
    </IconButton>
  );
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
      return { w: 352, h: 150 };
    case "terminal":
      return { w: 620, h: 380 };
    case "pin":
      return { w: 520, h: 600 };
    case "fold":
      return { w: 296, h: 96 };
    case "group":
      return { w: 296, h: 40 };
  }
}

/** A phone: the oldest request waiting for an answer, answerable right here with one tap. */
function PhoneWaiting({ client }: { client: AgentsClient }) {
  const data = client.data;
  const { requests } = rolling(data);
  const [folded, setFolded] = useState(false);
  const keyboard = useKeyboard();
  const q = requests[0];
  const t = data.threads.get(q.thread);
  // Folds to one line over the prompt bar, so the canvas under it can be read and moved (its zoom
  // bar moves to the top while this shows: `data-waiting`).
  if (folded)
    return (
      <button
        onClick={() => setFolded(false)}
        aria-label={`${requests.length} waiting for you: show`}
        className="rise flex h-11 w-full items-center gap-2 rounded-full border border-outline bg-raised px-4 text-left text-[13px] shadow-pop"
      >
        <span className="size-1.5 shrink-0 rounded-full bg-accent" aria-hidden />
        <span className="min-w-0 flex-1 truncate">{t?.value.title ?? "Waiting for you"}</span>
        {requests.length > 1 && <span className="shrink-0 font-mono text-[12px] text-accent">+{requests.length - 1}</span>}
        <ChevronUp className="size-4 shrink-0 text-muted" />
      </button>
    );
  return (
    <div
      className="rise relative max-h-[55dvh] overflow-y-auto rounded-2xl border border-outline bg-raised px-3.5 py-3 shadow-pop"
      // With the keyboard up, what's left of the screen above the prompt bar; typing an answer
      // keeps Previous and Submit in sight.
      style={keyboard.inset ? { maxHeight: Math.max(160, keyboard.visible - 132) } : undefined}
      onFocus={(e) => {
        if (!(e.target instanceof HTMLInputElement || e.target instanceof HTMLTextAreaElement)) return;
        const submit = e.target.closest("form")?.querySelector<HTMLElement>("button[type=submit]");
        requestAnimationFrame(() => submit?.scrollIntoView({ block: "nearest" }));
      }}
      aria-label="Waiting for you"
      data-phone-waiting
    >
      <button onClick={() => setFolded(true)} aria-label="Fold" title="Fold" className="absolute top-1.5 right-1.5 grid size-9 place-items-center rounded-full text-muted hover:text-fg">
        <ChevronDown className="size-4" />
      </button>
      <PendingRequest
        request={q}
        data={data}
        onOpen={() => setAgentsView({ kind: "thread", thread: q.thread })}
        extra={requests.length > 1 && <span className="shrink-0 font-mono text-accent">+{requests.length - 1}</span>}
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
