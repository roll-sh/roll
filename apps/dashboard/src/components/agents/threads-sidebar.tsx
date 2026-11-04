"use client";

/**
 * The threads sidebar left of the canvas (docs/code/agents.md, "Settling"), T3 Code's sidebar:
 * each project's threads, active first with their status, then the snoozed and settled shelves
 * (collapsed, remembered). A row opens its conversation in the side panel; its menu and the
 * conversation's settle, un-settle, snooze and wake. ⌘⇧S settles the open thread and opens the
 * next (T3's settle and advance), ⌘B shows or hides the sidebar. The rules are
 * `lib/agents-inbox.ts`; the daemon keeps the state.
 */

import { AlarmClock, AlarmClockOff, Check, ChevronDown, ChevronRight, MoreHorizontal, PanelLeft, PanelLeftClose, Plus, Undo2, X } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { MachineId, Thread } from "@/lib/agents-api";
import {
  cantPark,
  inbox,
  nextAfter,
  rowStatusLabel,
  SETTLED_FIRST,
  SETTLED_PAGE,
  shortAgo,
  snoozeFor,
  snoozeLeft,
  snoozePresets,
  type InboxRow,
  type ProjectInbox,
  type RowStatus,
  type SnoozeUnit,
} from "@/lib/agents-inbox";
import { runsOf, type AgentsData } from "@/lib/agents-store";
import { agentsSurface, setAgentsView } from "@/lib/agents-surface";
import type { MenuItem as MenuEntry } from "./menu";
import { Menu, MenuItem, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuShortcut, MenuSub, MenuSubPopup, MenuSubTrigger, MenuTrigger, Segmented } from "./ui";
import { useAgents } from "./use-agents";

const OPEN_KEY = "roll:agents:sidebar";
const SHELF_KEY = (shelf: "snoozed" | "settled") => `roll:agents:sidebar:${shelf}-expanded`;
const COLLAPSED_KEY = "roll:agents:sidebar:collapsed";

function stored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function store(key: string, value: string) {
  try {
    localStorage.setItem(key, value);
  } catch {
    // Not remembered.
  }
}

/** Whether the sidebar shows: closed until asked for (⌘B, the header button), then remembered on a computer. */
export function useSidebarOpen(phone: boolean): [boolean, (open: boolean) => void] {
  const [open, setOpen] = useState(false);
  useEffect(() => setOpen(!phone && stored(OPEN_KEY) === "1"), [phone]);
  const set = useCallback(
    (next: boolean) => {
      setOpen(next);
      if (!phone) store(OPEN_KEY, next ? "1" : "0");
    },
    [phone],
  );
  return [open, set];
}

/** The header's button that shows or hides the sidebar. */
export function SidebarToggle({ open, onToggle }: { open: boolean; onToggle: () => void }) {
  const label = `${open ? "Hide" : "Show"} threads (⌘B)`;
  return (
    <button
      type="button"
      onClick={onToggle}
      title={label}
      aria-label={label}
      aria-pressed={open}
      className={`grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg ${open ? "text-fg" : ""}`}
      data-nodrag
    >
      {open ? <PanelLeftClose className="size-4" strokeWidth={1.5} /> : <PanelLeft className="size-4" strokeWidth={1.5} />}
    </button>
  );
}

// ── actions ──

interface Toast {
  text: string;
  undo?: () => void;
  error?: boolean;
}

/** Settle, un-settle, snooze and wake a thread, with T3's undo toast; `toast` renders it. */
export function useInboxActions() {
  const { client, readOnly } = useAgents();
  const [toast, setToast] = useState<Toast | null>(null);
  useEffect(() => {
    if (!toast) return;
    const t = setTimeout(() => setToast(null), toast.error ? 6000 : 4500);
    return () => clearTimeout(t);
  }, [toast]);

  const send = useCallback(
    async (machine: MachineId, command: Parameters<typeof client.run>[1]) => {
      try {
        await client.run(machine, command);
        return true;
      } catch (e) {
        setToast({ text: e instanceof Error ? e.message : String(e), error: true });
        return false;
      }
    },
    [client],
  );
  const machineOf = useCallback((t: Thread) => client.data.threads.get(t.id)?.machine, [client]);

  /** Settle; when it's the open conversation, go on to the next thread (T3's settle and advance). */
  const settle = useCallback(
    async (t: Thread) => {
      const m = machineOf(t);
      if (!m || readOnly) return;
      const view = agentsSurface.get().view;
      const open = view.kind === "thread" && view.thread === t.id;
      // Planned before: the settled thread leaves the order.
      const next = open ? nextAfter(inbox(client.data, Date.now()), t.id) : null;
      if (!(await send(m, { type: "thread.settle", thread: t.id }))) return;
      if (open) setAgentsView(next ? { kind: "thread", thread: next } : { kind: "canvas" });
      setToast({ text: "Settled", undo: () => void send(m, { type: "thread.unsettle", thread: t.id }) });
    },
    [client, machineOf, readOnly, send],
  );
  const unsettle = useCallback(
    async (t: Thread) => {
      const m = machineOf(t);
      if (m && !readOnly) await send(m, { type: "thread.unsettle", thread: t.id });
    },
    [machineOf, readOnly, send],
  );
  const snooze = useCallback(
    async (t: Thread, until: number) => {
      const m = machineOf(t);
      if (!m || readOnly) return;
      if (await send(m, { type: "thread.snooze", thread: t.id, until })) {
        setToast({ text: `Snoozed until ${new Date(until).toLocaleString(undefined, { weekday: "short", hour: "2-digit", minute: "2-digit" })}`, undo: () => void send(m, { type: "thread.unsnooze", thread: t.id }) });
      }
    },
    [machineOf, readOnly, send],
  );
  const wake = useCallback(
    async (t: Thread) => {
      const m = machineOf(t);
      if (m && !readOnly) await send(m, { type: "thread.unsnooze", thread: t.id });
    },
    [machineOf, readOnly, send],
  );
  const autoSettle = useCallback(
    async (t: Thread, enabled: boolean) => {
      const m = machineOf(t);
      if (m && !readOnly) await send(m, { type: "thread.auto_settle", thread: t.id, enabled });
    },
    [machineOf, readOnly, send],
  );

  const toastEl = toast && (
    <div role="status" className="rise fixed bottom-[max(1rem,env(safe-area-inset-bottom))] left-1/2 z-[70] flex -translate-x-1/2 items-center gap-3 rounded-lg border border-outline px-3 py-2 text-[12.5px] shadow-pop inset-shadow-highlight glass">
      <span className={toast.error ? "text-danger" : "text-fg"}>{toast.text}</span>
      {toast.undo && (
        <button
          type="button"
          className="font-medium text-accent hover:underline"
          onClick={() => {
            toast.undo?.();
            setToast(null);
          }}
        >
          Undo
        </button>
      )}
    </div>
  );
  return { settle, unsettle, snooze, wake, autoSettle, toast: toastEl, readOnly: !!readOnly };
}

/**
 * The conversation menu's inbox items (settle or un-settle, snooze or wake) and what they open:
 * spread `items` into the menu and render `extra`.
 */
export function useInboxMenu(thread: Thread): { items: MenuEntry[]; extra: ReactNode } {
  const actions = useInboxActions();
  const [snoozing, setSnoozing] = useState(false);
  if (actions.readOnly) return { items: [], extra: null };
  const settled = thread.inbox.settle?.state === "settled";
  const snoozed = thread.inbox.snooze !== null;
  const items: MenuEntry[] = [
    settled
      ? { label: "Un-settle", icon: <Undo2 />, onSelect: () => void actions.unsettle(thread) }
      : { label: "Settle", icon: <Check />, disabled: !!cantPark(thread, "settle"), onSelect: () => void actions.settle(thread) },
    snoozed
      ? { label: "Wake now", icon: <AlarmClockOff />, onSelect: () => void actions.wake(thread) }
      : { label: "Snooze…", icon: <AlarmClock />, disabled: !!cantPark(thread, "snooze"), onSelect: () => setSnoozing(true) },
  ];
  const extra = (
    <>
      {snoozing && <SnoozeDialog presets onClose={() => setSnoozing(false)} onSnooze={(until) => void actions.snooze(thread, until)} />}
      {actions.toast}
    </>
  );
  return { items, extra };
}

// ── the sidebar ──

/**
 * The sidebar, and what works while it's hidden too: the shortcuts, and telling the daemon a
 * conversation was seen (`thread.visit`: when it opens, and when one of its runs ends while open).
 */
export function ThreadsSidebar({ data, open, setOpen, phone, openThread }: { data: AgentsData; open: boolean; setOpen: (open: boolean) => void; phone: boolean; openThread: string | null }) {
  const { client, startNew, readOnly } = useAgents();
  const actions = useInboxActions();
  const [now, setNow] = useState(() => Date.now());
  // Snoozes count down and "3h" moves on.
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(t);
  }, []);
  const groups = useMemo(() => inbox(data, now), [data, now]);

  // Seen: the open conversation, again when a run of it ends while it's open.
  const shown = openThread ? data.threads.get(openThread) : undefined;
  const lastEnded = useMemo(() => (openThread ? runsOf(data, openThread).reduce((at, r) => Math.max(at, r.completed_at ?? 0), 0) : 0), [data, openThread]);
  const sent = useRef("");
  useEffect(() => {
    if (!shown || readOnly) return;
    const t = shown.value;
    const key = `${t.id}:${lastEnded}:${t.inbox.woke_at ?? 0}`;
    if (sent.current.startsWith(`${t.id}:`) && sent.current === key) return;
    const fresh = !sent.current.startsWith(`${t.id}:`);
    const seen = t.inbox.visited_at ?? 0;
    sent.current = key;
    if (fresh || seen < lastEnded || seen < (t.inbox.woke_at ?? 0)) client.run(shown.machine, { type: "thread.visit", thread: t.id }).catch(() => {});
  }, [client, shown, lastEnded, readOnly]);

  // ⌘⇧S settles the open thread and advances (un-settles a settled one); ⌘B shows or hides this.
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || e.altKey || e.defaultPrevented) return;
      if ((e.target as HTMLElement | null)?.closest?.(".xterm")) return;
      const key = e.key.toLowerCase();
      if (e.shiftKey && key === "s") {
        const view = agentsSurface.get().view;
        const t = view.kind === "thread" ? client.data.threads.get(view.thread)?.value : undefined;
        if (!t) return;
        e.preventDefault();
        void (t.inbox.settle?.state === "settled" ? actions.unsettle(t) : actions.settle(t));
      } else if (!e.shiftKey && key === "b") {
        e.preventDefault();
        setOpen(!open);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [client, actions, open, setOpen]);

  const [collapsed, setCollapsed] = useState<Set<string>>(() => new Set());
  useEffect(() => {
    try {
      setCollapsed(new Set(JSON.parse(stored(COLLAPSED_KEY) ?? "[]") as string[]));
    } catch {
      // Nothing remembered.
    }
  }, []);
  const toggleProject = (id: string) =>
    setCollapsed((c) => {
      const next = new Set(c);
      if (!next.delete(id)) next.add(id);
      store(COLLAPSED_KEY, JSON.stringify([...next]));
      return next;
    });
  const [shelves, setShelves] = useState({ snoozed: false, settled: false });
  useEffect(() => setShelves({ snoozed: stored(SHELF_KEY("snoozed")) === "1", settled: stored(SHELF_KEY("settled")) === "1" }), []);
  const toggleShelf = (shelf: "snoozed" | "settled") =>
    setShelves((s) => {
      store(SHELF_KEY(shelf), s[shelf] ? "0" : "1");
      return { ...s, [shelf]: !s[shelf] };
    });

  const pick = (thread: string) => {
    setAgentsView({ kind: "thread", thread });
    if (phone) setOpen(false);
  };
  const active = groups.reduce((n, g) => n + g.active.length, 0);

  return (
    <>
      {actions.toast}
      {open && (
        <aside
          aria-label="Threads"
          className={`sidebar-in flex min-h-0 flex-col bg-bg ${phone ? "absolute inset-0 z-30" : "relative w-[268px] shrink-0"}`}
        >
          <div className="flex h-10 shrink-0 items-center gap-2 pr-1.5 pl-3">
            <h3 className="text-[12.5px] font-[590] tracking-[-0.01em]">Threads</h3>
            <span className="font-mono text-[11px] text-subtle">{active}</span>
            <span className="ml-auto flex items-center">
              {startNew && !readOnly && (
                <button type="button" onClick={() => startNew()} title="New agent" aria-label="New agent" className="grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg">
                  <Plus className="size-4" strokeWidth={1.5} />
                </button>
              )}
              <button type="button" onClick={() => setOpen(false)} title="Hide threads (⌘B)" aria-label="Hide threads" className="grid size-7 place-items-center rounded-md text-subtle transition hover:bg-panel-2 hover:text-fg">
                {phone ? <X className="size-4" strokeWidth={1.5} /> : <PanelLeftClose className="size-4" strokeWidth={1.5} />}
              </button>
            </span>
          </div>
          <div className="min-h-0 flex-1 overflow-y-auto px-1.5 pb-4" role="tree" aria-label="Projects">
            {groups.length === 0 && <p className="px-2 py-6 text-center text-[12.5px] text-subtle">No projects yet.</p>}
            {groups.map((g) => (
              <ProjectGroup
                key={g.project.id}
                group={g}
                machine={data.machines.get(g.machine)?.value.name}
                collapsed={collapsed.has(g.project.id)}
                onToggle={() => toggleProject(g.project.id)}
                shelves={shelves}
                onShelf={toggleShelf}
                openThread={openThread}
                onPick={pick}
                onNew={startNew && !readOnly ? () => startNew(g.project.id) : undefined}
                now={now}
                actions={actions}
              />
            ))}
          </div>
        </aside>
      )}
    </>
  );
}

type Actions = ReturnType<typeof useInboxActions>;

function ProjectGroup({
  group,
  machine,
  collapsed,
  onToggle,
  shelves,
  onShelf,
  openThread,
  onPick,
  onNew,
  now,
  actions,
}: {
  group: ProjectInbox;
  machine: string | undefined;
  collapsed: boolean;
  onToggle: () => void;
  shelves: { snoozed: boolean; settled: boolean };
  onShelf: (shelf: "snoozed" | "settled") => void;
  openThread: string | null;
  onPick: (thread: string) => void;
  onNew?: () => void;
  now: number;
  actions: Actions;
}) {
  const [settledShown, setSettledShown] = useState(SETTLED_FIRST);
  const row = (r: InboxRow) => <ThreadRow key={r.thread.id} row={r} selected={r.thread.id === openThread} onPick={onPick} now={now} actions={actions} />;
  const isOpen = (r: InboxRow) => r.thread.id === openThread;
  const hidden = group.settled.length - settledShown;
  return (
    <section className="mt-2" role="treeitem" aria-expanded={!collapsed} aria-label={group.project.name}>
      <div className="group/p flex h-7 items-center gap-1.5 rounded-md pr-1 pl-1.5 text-subtle">
        <button type="button" onClick={onToggle} className="flex min-w-0 flex-1 items-center gap-1.5 text-left hover:text-fg" aria-label={`${collapsed ? "Show" : "Hide"} ${group.project.name}`}>
          <Monogram name={group.project.name} />
          <span className="truncate text-[12px] font-medium text-muted">{group.project.name}</span>
          {machine && <span className="truncate text-[11px] text-subtle">{machine}</span>}
          {collapsed ? <ChevronRight className="size-3 shrink-0" /> : <ChevronDown className="size-3 shrink-0 opacity-0 group-hover/p:opacity-100" />}
        </button>
        {onNew && (
          <button type="button" onClick={onNew} title={`New agent in ${group.project.name}`} aria-label={`New agent in ${group.project.name}`} className="grid size-6 place-items-center rounded-md opacity-0 transition group-hover/p:opacity-100 hover:bg-panel-2 hover:text-fg focus-visible:opacity-100 [@media(hover:none)]:opacity-100">
            <Plus className="size-3.5" strokeWidth={1.5} />
          </button>
        )}
      </div>
      {!collapsed && (
        <div role="group" className="mt-0.5 space-y-px">
          {group.active.map(row)}
          {group.active.length === 0 && group.snoozed.length + group.settled.length === 0 && <p className="py-1 pl-8 text-[12px] text-subtle">No threads</p>}
          {group.snoozed.length > 0 && (
            <>
              <ShelfHeader label="Snoozed" count={group.snoozed.length} open={shelves.snoozed} onToggle={() => onShelf("snoozed")} />
              {(shelves.snoozed ? group.snoozed : group.snoozed.filter(isOpen)).map(row)}
            </>
          )}
          {group.settled.length > 0 && (
            <>
              <ShelfHeader label="Settled" count={group.settled.length} open={shelves.settled} onToggle={() => onShelf("settled")} />
              {(shelves.settled ? group.settled.slice(0, settledShown) : group.settled.filter(isOpen)).map(row)}
              {shelves.settled && hidden > 0 && (
                <button type="button" onClick={() => setSettledShown((n) => n + SETTLED_PAGE)} className="ml-7 rounded-md px-1.5 py-1 text-[12px] text-subtle transition hover:bg-panel-2 hover:text-fg">
                  Show {Math.min(hidden, SETTLED_PAGE)} more
                </button>
              )}
            </>
          )}
        </div>
      )}
    </section>
  );
}

function ShelfHeader({ label, count, open, onToggle }: { label: string; count: number; open: boolean; onToggle: () => void }) {
  return (
    <button type="button" onClick={onToggle} aria-expanded={open} className="mt-1 flex h-7 w-full items-center gap-1.5 rounded-md pr-2 pl-2.5 text-left text-[11.5px] text-subtle transition hover:bg-panel-2/60 hover:text-muted">
      {open ? <ChevronDown className="size-3" /> : <ChevronRight className="size-3" />}
      <span className="font-medium">{label}</span>
      <span className="font-mono text-[10.5px]">{count}</span>
    </button>
  );
}

/** T3's project monogram: the first letter on a quiet square. */
function Monogram({ name }: { name: string }) {
  return (
    <span aria-hidden className="grid size-4 shrink-0 place-items-center rounded-[4px] border border-line-2 bg-panel text-[9.5px] font-semibold text-muted uppercase">
      {name.trim().charAt(0) || "·"}
    </span>
  );
}

const STATUS_DOT: Record<RowStatus, string> = {
  working: "bg-accent pulse-dot",
  waiting: "bg-accent",
  failed: "bg-danger",
  woke: "bg-fg",
  done: "bg-ok",
};

function ThreadRow({ row, selected, onPick, now, actions }: { row: InboxRow; selected: boolean; onPick: (thread: string) => void; now: number; actions: Actions }) {
  const t = row.thread;
  const [menu, setMenu] = useState(false);
  const [custom, setCustom] = useState(false);
  const quiet = row.status === null || row.shelf !== "active";
  const pill = row.status === "waiting" || row.status === "failed" || row.status === "woke" ? row.status : null;
  const meta =
    row.shelf === "snoozed" ? (
      <span className="flex items-center gap-1" title={`Wakes ${new Date(row.at).toLocaleString()}`}>
        <AlarmClock className="size-3" strokeWidth={1.5} />
        {snoozeLeft(row.at, now)}
      </span>
    ) : (
      shortAgo(row.at, now)
    );
  const quick =
    row.shelf === "settled"
      ? { label: "Un-settle thread", icon: <Undo2 className="size-3.5" strokeWidth={1.5} />, run: () => actions.unsettle(t), disabled: false }
      : row.shelf === "snoozed"
        ? { label: "Wake thread now", icon: <AlarmClockOff className="size-3.5" strokeWidth={1.5} />, run: () => actions.wake(t), disabled: false }
        : { label: "Settle thread", icon: <Check className="size-3.5" strokeWidth={1.5} />, run: () => actions.settle(t), disabled: !!cantPark(t, "settle") };
  return (
    <div
      role="treeitem"
      aria-selected={selected}
      className={`group/r relative flex h-[30px] items-center rounded-md transition-colors duration-150 ${selected ? "bg-fg/[0.07] text-fg inset-shadow-highlight" : "hover:bg-fg/[0.04]"} ${menu ? "bg-fg/[0.04]" : ""}`}
    >
      <button type="button" onClick={() => onPick(t.id)} className="flex h-full min-w-0 flex-1 items-center gap-2 pr-1 pl-2.5 text-left" title={t.title}>
        <span className="grid size-3 shrink-0 place-items-center">
          {row.status === "woke" ? (
            <AlarmClock className="size-3 text-fg" strokeWidth={1.75} aria-hidden />
          ) : (
            <span aria-hidden className={`size-1.5 rounded-full ${row.status ? STATUS_DOT[row.status] : "bg-line-2"}`} />
          )}
        </span>
        <span className={`min-w-0 flex-1 truncate text-[13px] tracking-[-0.005em] ${selected ? "font-[510] text-fg" : quiet ? "text-muted" : "text-soft"}`}>{t.title}</span>
        {row.status && <span className="sr-only">{rowStatusLabel[row.status]}</span>}
      </button>
      <span className={`flex shrink-0 items-center gap-1 pr-2 font-mono text-[10.5px] text-subtle ${actions.readOnly ? "" : "group-hover/r:hidden"} ${menu ? "hidden" : ""}`}>
        {pill && (
          <span className={`rounded-full px-1.5 py-px font-sans text-[10.5px] ${pill === "failed" ? "bg-danger/12 text-danger" : pill === "waiting" ? "bg-accent/15 text-accent" : "bg-fg/[0.06] text-muted"}`}>
            {rowStatusLabel[pill]}
          </span>
        )}
        {!pill && meta}
      </span>
      {!actions.readOnly && (
        <span className={`shrink-0 items-center pr-1 ${menu ? "flex" : "hidden group-hover/r:flex [@media(hover:none)]:flex"}`}>
          <button type="button" title={quick.label} aria-label={quick.label} disabled={quick.disabled} onClick={() => void quick.run()} className="grid size-6 place-items-center rounded-md text-subtle transition hover:bg-panel hover:text-fg disabled:opacity-30">
            {quick.icon}
          </button>
          <RowMenu thread={t} row={row} open={menu} setOpen={setMenu} actions={actions} onPick={() => onPick(t.id)} onCustom={() => setCustom(true)} now={now} />
        </span>
      )}
      {custom && <SnoozeDialog onClose={() => setCustom(false)} onSnooze={(until) => void actions.snooze(t, until)} />}
    </div>
  );
}

/** A row's menu: T3's thread action menu, with its Snooze and Auto-settle behavior submenus. */
function RowMenu({
  thread,
  row,
  open,
  setOpen,
  actions,
  onPick,
  onCustom,
  now,
}: {
  thread: Thread;
  row: InboxRow;
  open: boolean;
  setOpen: (open: boolean) => void;
  actions: Actions;
  onPick: () => void;
  onCustom: () => void;
  now: number;
}) {
  const settleBlocked = cantPark(thread, "settle");
  const snoozeBlocked = cantPark(thread, "snooze");
  return (
    <Menu open={open} onOpenChange={setOpen}>
      <MenuTrigger
        aria-label="Thread actions"
        title="Thread actions"
        className={`grid size-6 place-items-center rounded-md text-subtle transition hover:bg-panel hover:text-fg ${open ? "bg-panel text-fg" : ""}`}
      >
        <MoreHorizontal className="size-3.5" strokeWidth={1.5} />
      </MenuTrigger>
      <MenuPopup align="end" className="w-56">
        <MenuItem onClick={onPick}>Open</MenuItem>
        {row.shelf === "settled" ? (
          <MenuItem onClick={() => void actions.unsettle(thread)}>
            <Undo2 />
            Un-settle thread
          </MenuItem>
        ) : (
          <MenuItem disabled={!!settleBlocked} title={settleBlocked ?? undefined} onClick={() => void actions.settle(thread)}>
            <Check />
            Settle thread
            <MenuShortcut>⌘⇧S</MenuShortcut>
          </MenuItem>
        )}
        {row.shelf === "snoozed" ? (
          <MenuItem onClick={() => void actions.wake(thread)}>
            <AlarmClockOff />
            Wake thread now
          </MenuItem>
        ) : snoozeBlocked ? (
          <MenuItem disabled title={snoozeBlocked}>
            <AlarmClock />
            Snooze
          </MenuItem>
        ) : (
          <MenuSub>
            <MenuSubTrigger>
              <AlarmClock />
              Snooze
            </MenuSubTrigger>
            <MenuSubPopup className="w-60">
              {snoozePresets(now).map((p) => (
                <MenuItem key={p.label} onClick={() => void actions.snooze(thread, p.until)}>
                  {p.label}
                  <span className="ms-auto text-[11px] text-subtle">{p.when}</span>
                </MenuItem>
              ))}
              <MenuSeparator />
              <MenuItem onClick={onCustom}>Custom…</MenuItem>
            </MenuSubPopup>
          </MenuSub>
        )}
        <MenuSeparator />
        <MenuSub>
          <MenuSubTrigger>Auto-settle behavior</MenuSubTrigger>
          <MenuSubPopup>
            <MenuRadioGroup value={thread.inbox.manual ? "disabled" : "enabled"} onValueChange={(v) => void actions.autoSettle(thread, v === "enabled")}>
              <MenuRadioItem value="enabled">Enabled</MenuRadioItem>
              <MenuRadioItem value="disabled">Disabled</MenuRadioItem>
            </MenuRadioGroup>
          </MenuSubPopup>
        </MenuSub>
      </MenuPopup>
    </Menu>
  );
}

// ── custom snooze ──

const UNITS: { id: SnoozeUnit; label: string }[] = [
  { id: "minutes", label: "Minutes" },
  { id: "hours", label: "Hours" },
  { id: "days", label: "Days" },
];

/** A local `datetime-local` value. */
function localInput(ms: number): string {
  const d = new Date(ms - new Date(ms).getTimezoneOffset() * 60_000);
  return d.toISOString().slice(0, 16);
}

/**
 * T3's custom snooze: a date and time, or a duration. `presets` lists T3's presets above it (the
 * conversation's menu has no submenu for them).
 */
export function SnoozeDialog({ onSnooze, onClose, presets = false }: { onSnooze: (until: number) => void; onClose: () => void; presets?: boolean }) {
  const [mode, setMode] = useState<"date" | "duration">("date");
  const [date, setDate] = useState(() => localInput(Date.now() + 3_600_000));
  const [amount, setAmount] = useState("2");
  const [unit, setUnit] = useState<SnoozeUnit>("hours");
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("keydown", onKey, true);
    return () => window.removeEventListener("keydown", onKey, true);
  }, [onClose]);
  const done = (until: number) => {
    onSnooze(until);
    onClose();
  };
  const submit = (e: React.FormEvent) => {
    e.preventDefault();
    const now = Date.now();
    if (mode === "date") {
      const at = new Date(date).getTime();
      if (!Number.isFinite(at) || at <= now) return setError("Choose a valid date and time in the future.");
      return done(at);
    }
    const until = snoozeFor(Number(amount), unit, now);
    if (until === null) return setError("Enter a positive duration.");
    done(until);
  };
  return (
    <div className="fade-in fixed inset-0 z-[60] grid place-items-center bg-(--scrim) p-4 backdrop-blur-[2px]" onPointerDown={(e) => e.target === e.currentTarget && onClose()}>
      <form role="dialog" aria-modal aria-label={presets ? "Snooze" : "Custom snooze"} onSubmit={submit} className="sheet-in w-full max-w-[380px] rounded-xl border border-line-2 bg-sheet p-4 shadow-sheet inset-shadow-highlight">
        <h2 className="text-[15px] font-[590] tracking-[-0.02em]">{presets ? "Snooze" : "Custom snooze"}</h2>
        <p className="mt-1 text-[12.5px] text-muted">Choose when snoozed threads return to your inbox.</p>
        {presets && (
          <div className="mt-3 grid gap-px">
            {snoozePresets(Date.now()).map((p) => (
              <button key={p.label} type="button" onClick={() => done(p.until)} className="flex items-center justify-between rounded-md px-2 py-1.5 text-left text-[13px] text-muted transition hover:bg-panel-2 hover:text-fg">
                {p.label}
                <span className="text-[11.5px] text-subtle">{p.when}</span>
              </button>
            ))}
            <div className="my-2 h-px bg-line" />
          </div>
        )}
        <Segmented
          label="Snooze by"
          value={mode}
          onChange={(m) => {
            setMode(m);
            setError(null);
          }}
          options={[
            { value: "date", label: "Date and time" },
            { value: "duration", label: "Duration" },
          ]}
          className="mt-3 w-full [&>*]:flex-1"
        />
        {mode === "date" ? (
          <input
            type="datetime-local"
            aria-label="Wake at"
            value={date}
            onChange={(e) => {
              setDate(e.target.value);
              setError(null);
            }}
            className="mt-3 h-9 w-full rounded-md border border-line bg-bg px-2.5 font-mono text-[13px] text-fg outline-none focus:border-accent/60"
          />
        ) : (
          <div className="mt-3 flex gap-2">
            <input
              inputMode="decimal"
              aria-label="How long"
              value={amount}
              onChange={(e) => {
                setAmount(e.target.value);
                setError(null);
              }}
              className="h-9 w-20 rounded-md border border-line bg-bg px-2.5 font-mono text-[13px] text-fg outline-none focus:border-accent/60"
            />
            <Segmented label="Unit" value={unit} onChange={setUnit} options={UNITS.map((u) => ({ value: u.id, label: u.label }))} className="flex-1 [&>*]:flex-1" />
          </div>
        )}
        {error && <p className="mt-2 text-[12px] text-danger">{error}</p>}
        <div className="mt-4 flex justify-end gap-2">
          <button type="button" onClick={onClose} className="h-8 rounded-md px-3 text-[12.5px] text-muted transition hover:bg-panel-2 hover:text-fg">
            Cancel
          </button>
          <button type="submit" className="h-8 rounded-md bg-fg px-3 text-[12.5px] font-medium text-bg transition hover:opacity-85">
            Snooze
          </button>
        </div>
      </form>
    </div>
  );
}
