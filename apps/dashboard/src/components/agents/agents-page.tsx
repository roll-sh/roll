"use client";

import { DotField, HorizonMark, RollMark } from "@roll/brand";
import { Archive, ArrowLeft, MoreHorizontal, PanelRightClose, Pin, PinOff, Plus, Search, Server, Settings, Share2, X, Zap } from "lucide-react";
import Link from "next/link";
import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { isBusy } from "@/lib/agents-api";
import { setActiveClient, type AgentsClient } from "@/lib/agents-client";
import { deriveGraph, rolling, type GraphNode } from "@/lib/agents-graph";
import { runsOf } from "@/lib/agents-store";
import type { Direction, Size } from "@/lib/agents-layout";
import { closePalette, noteThreadOpen, openPalette, togglePalette } from "@/lib/agents-palette";
import type { AgentsServices } from "@/lib/agents-services";
import { isNewAgentKey, setAgentsView, useAgentsSurface, viewFromHash, type AgentsView } from "@/lib/agents-surface";
import { ALL, applyDisplay, applyView, directionOf, focusOn, isPinned, pinKey, pinnedConversations, pinOf, projectOf, viewStorageKey, withDirection, type Pin as PinRec } from "@/lib/agents-views";
import { desktop, onDesktopDeepLink, useDesktop } from "@/lib/desktop";
import { LINK_PARAMS, type IntegrationsLink } from "@/lib/integrations-api";
import { useModelPrefsSync, useOwnerSettings } from "@/lib/owner-settings";
import { useMarkState, useMarkStateValue } from "../mark-state";
import { AddMachineDialog, AddMachinePaths } from "./add-machine";
import { AgentsCanvas, type CanvasHandle } from "./agents-canvas";
import { AutomationPanel } from "./automations/automation-panel";
import { AutomationsView } from "./automations/automations-view";
import { Notices } from "./automations/notices";
import { CommandPalette, type PaletteHost } from "./command-palette";
import { openIntegrations } from "./integrations";
import { Menu } from "./menu";
import { Launcher } from "./new-agent";
import { PinnedConversation, PinnedDiff, PinnedTerminal } from "./pinned";
import { MachineNode, ProjectNode, RequestNode, SubagentNode, ThreadNode } from "./nodes";
import { PromptBar } from "./prompt-bar";
import { PendingRequest } from "./pending-request";
import { SettingsPanel } from "./settings-panel";
import { useSandboxes } from "./sandbox";
import { Conversation, IconButton } from "./session";
import { closeTopLayer } from "./escape-layers";
import { CloseTerminal, TerminalNode } from "./terminal";
import { threadDone } from "./thread-status";
import { SidebarToggle, ThreadsSidebar, useSidebarOpen } from "./threads-sidebar";
import { setShelfOpen } from "@/lib/agents-shelf";
import { AgentsContext, useAgents, useAgentsSnapshot, useHold, useMarkDriver, type AgentsCtx } from "./use-agents";
import { useAgentsConnection } from "./use-connection";
import { PinTools, useCanvasViews, ViewsBar, type CanvasViews } from "./views";

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
  const { client, services, error } = useAgentsConnection({ owner, demo });
  // Where the window opens (`?new`, `?focus=t:thr_…`, `?settings=integrations`, or a `#agents/…`
  // link as the popup takes them), before the first paint.
  useLayoutEffect(() => {
    const linked = viewFromHash(window.location.hash);
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
  const mac = typeof window !== "undefined" && desktop()?.window === "agents" && desktop()?.platform === "darwin";
  return (
    <div className="h-dvh bg-bg text-fg" data-agents data-desktop={mac ? "agents-mac" : undefined}>
      {client && services ? (
        <AgentsSurface client={client} services={services} demo={demo} chrome="window" focus={focus && !focus.startsWith("t:") ? focus : undefined} />
      ) : (
        <CenterNote shimmer={!error}>{error ?? "Connecting…"}</CenterNote>
      )}
    </div>
  );
}

/** Connecting, or why it can't: the mark thinking (or still) over a line of text. */
export function CenterNote({ children, shimmer = false }: { children: ReactNode; shimmer?: boolean }) {
  return (
    <div className="fade-in grid h-full place-items-center px-6 text-center">
      <div className="flex flex-col items-center gap-4 text-fg">
        <HorizonMark size={40} state={shimmer ? "thinking" : "error"} label="" />
        <span className={`text-[13px] ${shimmer ? "shimmer" : "text-muted"}`}>{children}</span>
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
  const [adding, setAdding] = useState(false);
  const [showArchived, setShowArchived] = useState(false);
  const [sidebar, setSidebar] = useSidebarOpen(phone);
  const canvas = useRef<CanvasHandle>(null);
  // The prompt bar's dock: the canvas keeps cards clear of it.
  const dock = useRef<HTMLDivElement>(null);
  const views = useCanvasViews(services.canvas, client.owner, data.canvas?.revision ?? 0);
  const root = useRef<HTMLDivElement>(null);
  const pendingFocus = useRef<string | null>(initialFocus ?? null);

  const threadOpen = view.kind === "thread" ? data.threads.get(view.thread) : undefined;
  // A conversation that's gone (deleted, or never loaded here) falls back to the canvas.
  const gone = useGone(view.kind === "thread" ? view.thread : null, !!threadOpen, snap.conn === "live");
  useEffect(() => {
    if (gone) setAgentsView({ kind: "canvas" });
  }, [gone]);
  const shown: AgentsView = gone ? { kind: "canvas" } : view;
  // The card beside the open panel, selected on the canvas.
  const selected = shown.kind === "thread" ? `t:${shown.thread}` : shown.kind === "settings" && shown.target !== "owner" ? shown.target : undefined;
  useEffect(() => {
    if (!selected) return;
    // Once the panel has made the canvas narrower.
    const t = setTimeout(() => canvas.current?.reveal(selected), 280);
    return () => clearTimeout(t);
  }, [selected]);

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

  const openThread = useCallback((thread: string) => {
    // How often and how lately, for the command palette's ranking.
    noteThreadOpen(thread);
    setAgentsView({ kind: "thread", thread });
  }, []);
  const close = useCallback(() => setAgentsView({ kind: "canvas" }), []);

  // Focus comes back to the canvas when a conversation, settings or the launcher closes (it was
  // lost with the panel): the card of what was open, else what had focus on the canvas before.
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
    const key = before.kind === "thread" ? `t:${before.thread}` : before.kind === "settings" && before.target !== "owner" ? before.target : null;
    const t = setTimeout(() => {
      const a = document.activeElement;
      if (a && a !== document.body && a.isConnected) return;
      const card = key ? root.current?.querySelector<HTMLElement>(`[data-card][data-key="${CSS.escape(key)}"]`) : null;
      const back = card ?? (lastCanvasFocus.current?.isConnected ? lastCanvasFocus.current : null);
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

  // One project filling the canvas (per device, until Escape or the chip's ×).
  const [focused, setFocused] = useState<string | null>(null);
  const focusProject = useCallback((project: string | null) => {
    setFocused(project);
    if (project) setAgentsView({ kind: "canvas" });
    // Its settled and archived conversations open on its card (docs/code/agents.md, "Settling").
    if (project) setShelfOpen(project, true);
  }, []);
  const focusedGone = focused !== null && !data.projects.has(focused);
  useEffect(() => {
    if (focusedGone) setFocused(null);
  }, [focusedGone]);
  const prefs = useOwnerSettings(services?.settings, client.owner, data.settings?.revision ?? 0);
  // The model picker's favourites and recent models follow the owner (a share's replay has no services).
  useModelPrefsSync(prefs, services ? prefs.save : null);
  const ctx: AgentsCtx = useMemo(
    () => ({ client, owner: client.owner, demo, openThread, close, startNew, focus, focusProject, services, sandboxes, phone, prefs }),
    [client, demo, openThread, close, startNew, focus, focusProject, services, sandboxes, phone, prefs],
  );

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
  // an empty composer (it has focus in a conversation, so Escape would never leave otherwise).
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
      const typing = el && (el.isContentEditable || /^(INPUT|SELECT)$/.test(el.tagName) || (el.tagName === "TEXTAREA" && (el as HTMLTextAreaElement).value !== ""));
      if (typing) return;
      // An open automation closes to the list first; the list goes back to the canvas.
      if (shown.kind === "automations" && shown.open) setAgentsView({ kind: "automations", list: shown.list });
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

  const { busy: working, requests } = rolling(data);
  const toggleAutomations = useCallback(() => setAgentsView(shown.kind === "automations" ? { kind: "canvas" } : { kind: "automations" }), [shown.kind]);
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
      focused,
      focusProject,
    }),
    [showArchived, archived, views.views, views.current.id, views.select, direction, setDirection, focused, focusProject],
  );
  const end = (
    <>
      <button
        onClick={() => startNew()}
        className="ml-1 inline-flex h-7 items-center gap-1.5 rounded-md bg-fg pr-2.5 pl-2 text-[12.5px] font-medium tracking-[-0.01em] text-bg shadow-card inset-shadow-[0_1px_0_rgb(255_255_255/0.25)] transition-[opacity,scale] duration-150 hover:opacity-90 active:scale-[0.97]"
        title="New agent (⌘N)"
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
    <AgentsContext.Provider value={ctx}>
      <div ref={root} className="relative flex h-full min-h-0 flex-col bg-bg text-fg">
        {/* The full window drives the logo itself (in the popup, the header already does). */}
        {chrome === "window" && <MarkDriver client={client} />}
        <div className="flex min-h-0 flex-1 flex-col">
          <header className="chrome-in relative z-10 flex h-12 shrink-0 items-center gap-3 pr-2 pl-2 max-sm:gap-2 max-sm:pr-1 max-sm:pl-1" data-drag-region>
            <span className="flex shrink-0 items-center gap-2.5">
              <SidebarToggle open={sidebar} onToggle={() => setSidebar(!sidebar)} />
              {chrome === "window" ? <WindowHome /> : <HeaderMark />}
              <h2 className="text-[14px] font-[590] tracking-[-0.02em]">Agents</h2>
            </span>
            <span className="h-4 w-px shrink-0 bg-line-2 max-sm:hidden" aria-hidden />
            <span className="flex min-w-0 items-center gap-2 text-[12.5px] text-muted">
              <span className={`size-1.5 shrink-0 rounded-full ${snap.conn === "live" ? (running ? "pulse-dot bg-accent" : data.machines.size ? "bg-ok" : "bg-subtle") : snap.conn === "error" ? "bg-danger" : "bg-subtle"}`} aria-hidden />
              <span className="truncate">
                {snap.conn === "live" ? (running ? `${running} running` : data.machines.size ? "All idle" : "No machines yet") : snap.conn === "error" ? (snap.error ?? "Can't reach roll") : "Reconnecting…"}
              </span>
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
                <IconButton label={shown.kind === "automations" ? "Back to the canvas" : "Automations"} active={shown.kind === "automations"} onClick={toggleAutomations}>
                  <Zap className="size-4" strokeWidth={1.75} />
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
                  phone && { label: shown.kind === "automations" ? "Back to the canvas" : "Automations", icon: <Zap />, onSelect: toggleAutomations },
                  phone && { label: "Add a machine", icon: <Server />, onSelect: () => setAdding(true) },
                  phone && { label: "Agents settings", icon: <Settings />, onSelect: () => setAgentsView({ kind: "settings", target: "owner" }) },
                  archived > 0 && { label: `${showArchived ? "Hide" : "Show"} archived (${archived})`, icon: <Archive />, onSelect: () => setShowArchived((s) => !s) },
                  !demo && { label: "Shared links", icon: <Share2 />, onSelect: () => window.location.assign(`/dashboard/agents/shares?owner=${encodeURIComponent(client.owner)}`) },
                ]}
              />
              {end}
            </span>
          </header>
          <div className="relative flex min-h-0 flex-1">
            <ThreadsSidebar data={data} open={sidebar} setOpen={setSidebar} phone={phone} openThread={shown.kind === "thread" ? shown.thread : null} />
            {/* The working area: an inset panel one step up from the chrome (Linear's inverted L). */}
            <div className={`relative min-h-0 min-w-0 flex-1 overflow-hidden bg-canvas ${phone ? "" : "mx-2 mb-2 rounded-xl border border-line"}`}>
              {data.machines.size === 0 && snap.conn !== "connecting" && shown.kind !== "automations" ? (
                <Empty demo={demo} onCreated={onSandbox} />
              ) : (
                <>
                  {shown.kind === "automations" && <AutomationsView view={shown} phone={phone} />}
                  {/* Kept mounted under the automations, so the canvas comes back where it was. */}
                  <div className={shown.kind === "automations" ? "hidden" : "contents"}>
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
                      visible={shown.kind !== "new" && shown.kind !== "automations"}
                      selected={selected}
                      phone={phone}
                      onLaunch={onLaunch}
                      keepClear={dock}
                    />
                  </div>
                </>
              )}
              {launchError && (
                <p role="alert" className="rise absolute bottom-4 left-4 z-10 rounded-lg border border-danger/40 bg-panel px-3 py-2 text-[12px] text-danger">
                  {launchError}
                </p>
              )}
              {/* Beside an open conversation too (the canvas narrows; the draft stays); on a phone, under what's waiting. */}
              {data.machines.size > 0 && shown.kind !== "automations" && <PromptBar ref={dock} above={phone && shown.kind === "canvas" && requests.length > 0 && <PhoneWaiting client={client} />} />}
            </div>
            {shown.kind === "thread" && threadOpen && (
              <SidePanel phone={phone} label="Conversation">
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
              </SidePanel>
            )}
            {shown.kind === "settings" && (
              <SidePanel phone={phone} label="Settings">
                <SettingsPanel key={shown.target} target={shown.target} onClose={close} />
              </SidePanel>
            )}
            {shown.kind === "automations" && shown.open && (
              <SidePanel phone={phone} label="Automation">
                <AutomationPanel key={`${shown.open}:${shown.template ?? ""}`} view={{ ...shown, open: shown.open }} onClose={() => setAgentsView({ kind: "automations", list: shown.list })} />
              </SidePanel>
            )}
          </div>
        </div>
        <Notices />
        {adding && <AddMachineDialog onClose={() => setAdding(false)} onCreated={onSandbox} />}
        {shown.kind === "new" && <Launcher key={`${shown.project ?? ""}|${shown.machine ?? ""}|${shown.at?.x ?? ""},${shown.at?.y ?? ""}`} project={shown.project} machine={shown.machine} at={shown.at} onClose={close} />}
        <CommandPalette host={host} />
      </div>
    </AgentsContext.Provider>
  );
}

const PANEL_WIDTH = "roll:agents:panel-width";

/**
 * What's open beside the canvas (a conversation, settings): docked on the right, sliding in, as
 * wide as it was last dragged to; on a phone it covers the canvas.
 */
function SidePanel({ phone, label, children }: { phone: boolean; label: string; children: ReactNode }) {
  const [width, setWidth] = useState(() => {
    const saved = typeof window === "undefined" ? NaN : Number(localStorage.getItem(PANEL_WIDTH));
    return Number.isFinite(saved) && saved > 0 ? saved : 640;
  });
  const clamp = (w: number) => Math.round(Math.max(380, Math.min(w, window.innerWidth - 320)));
  const onPointerDown = (e: React.PointerEvent) => {
    e.preventDefault();
    const x0 = e.clientX;
    const w0 = width;
    const el = e.currentTarget as HTMLElement;
    el.setPointerCapture(e.pointerId);
    let last = w0;
    const move = (m: PointerEvent) => setWidth((last = clamp(w0 + x0 - m.clientX)));
    const up = () => {
      el.removeEventListener("pointermove", move);
      el.removeEventListener("pointerup", up);
      try {
        localStorage.setItem(PANEL_WIDTH, String(last));
      } catch {
        // Not remembered.
      }
    };
    el.addEventListener("pointermove", move);
    el.addEventListener("pointerup", up);
  };
  return (
    <aside
      aria-label={label}
      data-panel
      className={`panel-in flex min-h-0 flex-col ${phone ? "absolute inset-0 z-20 bg-sheet" : "relative shrink-0 pr-2 pb-2"}`}
      style={phone ? undefined : { width: `min(${width}px, calc(100% - 320px))` }}
    >
      {!phone && (
        <div
          role="separator"
          aria-orientation="vertical"
          aria-label="Resize the panel"
          onPointerDown={onPointerDown}
          className="group/resize absolute inset-y-0 -left-2 z-10 flex w-2 cursor-col-resize justify-center"
        >
          <span className="my-auto h-10 w-[3px] rounded-full bg-line-3 opacity-0 transition-[opacity,height,background-color] duration-150 group-hover/resize:h-16 group-hover/resize:bg-accent/70 group-hover/resize:opacity-100" />
        </div>
      )}
      {phone ? (
        children
      ) : (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden rounded-xl border border-line-2 bg-sheet shadow-sheet inset-shadow-highlight">{children}</div>
      )}
    </aside>
  );
}

function MarkDriver({ client }: { client: AgentsClient }) {
  useMarkDriver(useAgentsSnapshot(client));
  return null;
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

function CanvasView({
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
  phone,
  onLaunch,
  keepClear,
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
  phone: boolean;
  onLaunch?: (source: string, at: { x: number; y: number }) => void;
  keepClear?: React.RefObject<HTMLElement | null>;
}) {
  const { client, owner } = useAgents();
  const view = views.current;
  const filter = views.filter;
  // A pinned conversation answers its thread's requests itself.
  const expanded = useMemo(() => pinnedConversations(view), [view]);
  // The open conversation's terminals are in its drawer: not a second time on the canvas.
  const docked = selected?.startsWith("t:") ? selected.slice(2) : null;
  const whole = useMemo(() => deriveGraph(data, { showArchived, expanded, docked }), [data, showArchived, expanded, docked]);
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
  const graph = useMemo(() => (focused ? focusOn(viewed, `p:${focused}`) : viewed), [viewed, focused]);
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
      // Not in this view: All has everything.
      if (view.id !== ALL && whole.nodes.has(k)) views.select(ALL);
      return;
    }
    pendingFocus.current = null;
    for (const ms of [60, 420]) setTimeout(() => canvasRef.current?.focus(k), ms);
  });
  const exists = (p: PinRec) => (p.kind === "terminal" ? data.terminals.has(p.id) : data.threads.has(p.id));
  const toggle = (kind: PinRec["kind"], id: string) => views.togglePin(kind, id, exists);
  const machineOf = (n: GraphNode) => data.machines.get(n.machine)?.value;
  const render = (n: GraphNode) => {
    switch (n.kind) {
      case "machine":
        return <MachineNode machine={data.machines.get(n.id)!.value} />;
      case "project":
        return <ProjectNode project={data.projects.get(n.id)!.value} machine={machineOf(n)} />;
      case "thread":
        return (
          <PinTools
            view={view}
            onToggle={toggle}
            items={[
              { kind: "conversation", id: n.id, label: "the conversation" },
              { kind: "diff", id: n.id, label: "the diff" },
            ]}
          >
            <ThreadNode thread={data.threads.get(n.id)!.value} machine={machineOf(n)} />
          </PinTools>
        );
      case "subagent": {
        const node = [...data.nodes.values()].map((m) => m.get(n.id)).find(Boolean)?.value;
        return node ? <SubagentNode node={node} /> : null;
      }
      case "request":
        return <RequestNode request={data.requests.get(n.id)!.value} />;
      case "terminal":
        return (
          <PinTools view={view} onToggle={toggle} items={[{ kind: "terminal", id: n.id, label: "the terminal" }]} extra={<CloseTerminal terminal={data.terminals.get(n.id)!.value} machine={machineOf(n)} />}>
            <TerminalNode terminal={data.terminals.get(n.id)!.value} machine={machineOf(n)} />
          </PinTools>
        );
      case "pin": {
        const p = pinOf(view, n.key);
        if (!p) return null;
        const props = { pin: p, onUnpin: () => toggle(p.kind, p.id), onResize: (w: number, h: number) => views.resize(p, w, h) };
        if (p.kind === "terminal") {
          const t = data.terminals.get(p.id)?.value;
          return t ? <PinnedTerminal {...props} terminal={t} machine={machineOf(n)} /> : null;
        }
        const t = data.threads.get(p.id)?.value;
        if (!t) return null;
        return p.kind === "conversation" ? <PinnedConversation {...props} thread={t} machine={machineOf(n)} /> : <PinnedDiff {...props} thread={t} machine={machineOf(n)} />;
      }
    }
  };
  const sizeOf = useCallback(
    (n: GraphNode): Size => {
      if (n.kind !== "pin") return defaultSize(n);
      const p = pinOf(view, n.key);
      return p ? { w: p.w, h: p.h } : { w: 520, h: 600 };
    },
    [view],
  );
  const empty = graph.nodes.size === 0 && whole.nodes.size > 0;
  return (
    <div className="flex h-full min-h-0 flex-col">
      <ViewsBar views={views} data={data} active={visible} phone={phone} focused={focused ? { name: data.projects.get(focused)?.value.name ?? "project", clear: () => onFocus(null) } : undefined} />
      <div className="relative min-h-0 flex-1">
        <AgentsCanvas
          ref={canvasRef}
          graph={graph}
          storageKey={viewStorageKey(owner, view.id)}
          order={view.order}
          onOrder={views.setOrder}
          fitKey={`${JSON.stringify(filter)}|${focused ?? ""}|${display.group ?? ""}|${display.hide?.join() ?? ""}|${display.density ?? ""}`}
          direction={direction}
          onDirection={onDirection}
          density={display.density}
          onOpen={onOpen}
          render={render}
          defaultSize={sizeOf}
          active={visible}
          selected={selected}
          onLaunch={onLaunch}
          keepClear={keepClear}
          overlay={empty && <EmptyView name={view.id === ALL ? null : view.name} waitingOnly={filter.status?.length === 1 && filter.status[0] === "waiting"} onAll={() => (view.id === ALL ? views.setFilter({}) : views.select(ALL))} />}
        />
      </div>
    </div>
  );
}

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
  }
}

/** A phone: the oldest request waiting for an answer, answerable right here with one tap. */
function PhoneWaiting({ client }: { client: AgentsClient }) {
  const data = client.data;
  const { requests } = rolling(data);
  const q = requests[0];
  const t = data.threads.get(q.thread);
  return (
    <div className="rise max-h-[55dvh] overflow-y-auto rounded-2xl border border-outline bg-raised px-3.5 py-3 shadow-pop" aria-label="Waiting for you">
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
