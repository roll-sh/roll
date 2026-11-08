"use client";

/**
 * The threads as Linear's list and board (docs/code/agents.md, "Working at scale"): the same threads
 * the canvas shows for the view, filtered and focused the same way, grouped, sub-grouped and sorted
 * as the view's display says (`lib/agents-collection.ts`). This file is the host both share (rows,
 * groups, the collapsed groups, the keyboard cursor) and the list: a dense table drawn only where
 * it shows (a few dozen rows whatever the count), sortable from its headers, columns resizable,
 * keyboard-first (j/k or ↑/↓ move, Enter opens, x selects, Shift+j/k extends, ⌘A selects all, ←/→
 * fold a group). On a phone the rows are cards; Select (or a long press) picks several.
 */

import { ArrowDown, ArrowUp, Check, ChevronRight, Minus } from "lucide-react";
import { memo, useCallback, useDeferredValue, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useReducedMotion } from "@roll/brand";
import type { Request, ThreadId } from "@/lib/agents-api";
import { firstDir, flatten, groupRows, idsOf, namesOf, orderOf, readyToLand, rowsOf, sortRows, type Group, type Item, type Names, type ThreadRow } from "@/lib/agents-collection";
import { shortAgo } from "@/lib/agents-inbox";
import { projectColor } from "@/lib/agents-identity";
import { selectFromClick, selection, useSelected, useSelection } from "@/lib/agents-selection";
import type { AgentsData } from "@/lib/agents-store";
import { groupByOf, sortOf, subGroupByOf, type Display, type GroupBy, type Layout, type SortKey, type ViewFilter } from "@/lib/agents-views";
import { tint } from "./identity";
import { compact, providerName, ThreadChanges } from "./shared";
import { StatusPill, threadPill } from "./thread-status";
import { ThreadBoard } from "./thread-board";
import { useAgents } from "./use-agents";
import { onThreadContextMenu, openThreadMenu, ThreadMenuButton } from "./thread-menu";

// ── the host ──

export interface Collection {
  rows: ThreadRow[];
  groups: Group[];
  names: Names;
  /** Pending requests by thread (the status pill). */
  pending: Map<ThreadId, Request[]>;
  collapsed: ReadonlySet<string>;
  toggleGroup: (key: string) => void;
  /** The open conversation's thread. */
  open: ThreadId | null;
  openThread: (id: ThreadId) => void;
  phone: boolean;
  /** Phone: taps select instead of opening. */
  editing: boolean;
  setEditing: (on: boolean) => void;
  sort: ReturnType<typeof sortOf>;
  setSort: (key: SortKey) => void;
  /** What the groups are by (the board's columns). */
  by: GroupBy;
  /** Settle threads (cards dropped on Done). */
  settle: (ids: readonly ThreadId[]) => void;
  now: number;
}

/** Rows, groups and what the list and the board share, for the view showing. */
export function useCollection({
  data,
  filter,
  display,
  setDisplay,
  showArchived,
  focused,
  open,
  phone,
  storageKey,
  settle,
}: {
  data: AgentsData;
  filter: ViewFilter;
  display: Display;
  setDisplay: (d: Display) => void;
  showArchived: boolean;
  focused: string | null;
  open: ThreadId | null;
  phone: boolean;
  /** Where this view's collapsed groups are remembered on this device. */
  storageKey: string;
  settle: (ids: readonly ThreadId[]) => void;
}): Collection {
  const { openThread } = useAgents();
  // "3 min ago" moves on.
  const [now, setNow] = useState(() => Date.now());
  useEffect(() => {
    const t = setInterval(() => setNow(Date.now()), 30_000);
    return () => clearInterval(t);
  }, []);
  // The table follows the stream in an interruptible render: a burst of records never blocks a scroll or a key.
  const live = useDeferredValue(data);
  const sort = sortOf(display);
  const by = groupByOf(display);
  const sub = subGroupByOf(display);
  const names = useMemo(() => namesOf(live), [live]);
  const rows = useMemo(() => sortRows(rowsOf(live, { now, showArchived, filter, focused }), sort, names), [live, now, showArchived, filter, focused, sort, names]);
  const groups = useMemo(() => groupRows(rows, by, sub, names), [rows, by, sub, names]);
  const pending = useMemo(() => {
    const out = new Map<ThreadId, Request[]>();
    for (const r of live.requests.values()) if (r.value.status === "pending") out.set(r.value.thread, [...(out.get(r.value.thread) ?? []), r.value]);
    return out;
  }, [live.requests]);
  const [collapsed, setCollapsed] = useState<ReadonlySet<string>>(() => loadCollapsed(storageKey));
  useEffect(() => setCollapsed(loadCollapsed(storageKey)), [storageKey]);
  const toggleGroup = useCallback(
    (key: string) =>
      setCollapsed((c) => {
        const next = new Set(c);
        if (next.has(key)) next.delete(key);
        else next.add(key);
        saveCollapsed(storageKey, next);
        return next;
      }),
    [storageKey],
  );
  const [editing, setEditing] = useState(false);
  const selected = useSelection();
  // Edit mode ends with the selection (the bar's ×, Escape, an action that went through).
  useEffect(() => {
    if (!selected.ids.size) setEditing(false);
  }, [selected.ids.size]);
  const setSort = useCallback(
    (key: SortKey) => {
      const next = sort.key === key ? { key, dir: sort.dir === "asc" ? ("desc" as const) : ("asc" as const) } : { key, dir: firstDir(key) };
      setDisplay({ ...display, sort: next });
    },
    [sort, display, setDisplay],
  );
  return { rows, groups, names, pending, collapsed, toggleGroup, open, openThread, phone, editing, setEditing, sort, setSort, by, settle, now };
}

const COLLAPSED = (key: string) => `roll:agents:collapsed:${key}`;

function loadCollapsed(key: string): Set<string> {
  try {
    const v = JSON.parse(localStorage.getItem(COLLAPSED(key)) ?? "[]") as unknown;
    return new Set(Array.isArray(v) ? v.filter((x): x is string => typeof x === "string").slice(0, 200) : []);
  } catch {
    return new Set();
  }
}

function saveCollapsed(key: string, set: ReadonlySet<string>) {
  try {
    if (set.size) localStorage.setItem(COLLAPSED(key), JSON.stringify([...set]));
    else localStorage.removeItem(COLLAPSED(key));
  } catch {
    // Not remembered.
  }
}

/** The list or the board for the view showing. */
export function ThreadCollection({ layout, collection }: { layout: Exclude<Layout, "canvas">; collection: Collection }) {
  if (!collection.rows.length) return <Nothing />;
  const shown = layout === "board" ? <ThreadBoard c={collection} /> : <ThreadList c={collection} />;
  if (!collection.phone) return shown;
  // A phone has no ⌘ or Shift: Select (as a long press does) makes taps pick threads.
  const { editing, setEditing } = collection;
  return (
    <div className="flex h-full flex-col">
      <div className="flex h-9 shrink-0 items-center justify-between px-3 text-[12.5px] text-muted">
        <span className="font-mono text-[11.5px] tabular-nums">
          {collection.rows.length} thread{collection.rows.length === 1 ? "" : "s"}
        </span>
        <button
          type="button"
          onClick={() => {
            if (editing) selection.clear();
            setEditing(!editing);
          }}
          className="-mr-1.5 h-8 rounded-md px-2 font-medium text-fg active:bg-fg/[0.06]"
        >
          {editing ? "Cancel" : "Select"}
        </button>
      </div>
      <div className="min-h-0 flex-1">{shown}</div>
    </div>
  );
}

function Nothing() {
  return (
    <div className="fade-in grid h-full place-items-center px-6 text-center">
      <div className="space-y-1.5">
        <p className="text-[13.5px] font-medium tracking-[-0.01em]">No threads here</p>
        <p className="text-[12.5px] text-subtle">Threads show up as soon as they match the view.</p>
      </div>
    </div>
  );
}

// ── shared bits ──

/** A thread's status in the list and on a board card: T3's pill, or what it last did. */
export function RowStatus({ row, c }: { row: ThreadRow; c: Pick<Collection, "pending"> }) {
  const pill = threadPill(row.thread, c.pending.get(row.id) ?? [], row.last, row.thread.plan_ready);
  if (pill) return <StatusPill pill={pill} />;
  if (row.bucket === "queued") return <StatusPill pill={{ label: `${row.thread.queued.length} queued`, tone: "muted", pulse: false }} />;
  if (row.bucket === "done" && readyToLand(row.thread)) return <StatusPill pill={{ label: "Ready to land", tone: "ok", pulse: false }} />;
  return <span className="text-[10.5px] text-subtle">{row.thread.status === "idle" ? "Idle" : row.thread.status}</span>;
}

export function PrChip({ row }: { row: ThreadRow }) {
  const ch = row.thread.change;
  if (!ch) return null;
  const tone = ch.status === "ready" || ch.status === "landed" ? "text-ok" : ch.status === "failing" || ch.status === "conflict" ? "text-danger" : "text-muted";
  return (
    <a href={ch.url} onClick={(e) => e.stopPropagation()} title={ch.detail ?? ch.status} className={`inline-flex min-w-0 items-center gap-1 font-mono text-[11px] tabular-nums hover:underline ${tone}`}>
      #{ch.number}
      <span className="truncate font-sans text-[10.5px]">{ch.status}</span>
    </a>
  );
}

export function costOf(row: ThreadRow): string {
  const u = row.thread.usage;
  if (u.cost_usd != null && u.cost_usd > 0) return `$${u.cost_usd < 0.01 ? u.cost_usd.toFixed(3) : u.cost_usd.toFixed(2)}`;
  return row.tokens ? `${compact(row.tokens)} tok` : "";
}

/** The checkbox at a row's or card's start: shown on hover, always once anything is selected. */
export function Pick({ id, order, any, className = "" }: { id: ThreadId; order: () => readonly ThreadId[]; any: boolean; className?: string }) {
  const on = useSelected(id);
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={on}
      aria-label={on ? "Deselect" : "Select"}
      tabIndex={-1}
      onClick={(e) => {
        e.stopPropagation();
        if (e.shiftKey) selection.rangeTo(id, order());
        else selection.toggle(id);
      }}
      className={`grid size-4 shrink-0 place-items-center rounded-[4px] border transition-[opacity,background-color,border-color] duration-150 ${on ? "border-accent bg-accent text-white" : `border-line-3 bg-transparent text-transparent ${any ? "opacity-100" : "opacity-0 group-hover/row:opacity-100 focus-visible:opacity-100"}`} ${className}`}
    >
      <Check className="size-3" strokeWidth={2.5} />
    </button>
  );
}

/** A group's checkbox: all, some (a dash) or none of its threads selected. */
export function GroupPick({ group }: { group: Group }) {
  const sel = useSelection();
  const ids = idsOf(group);
  const n = ids.filter((id) => sel.ids.has(id)).length;
  const all = n > 0 && n === ids.length;
  return (
    <button
      type="button"
      role="checkbox"
      aria-checked={all ? true : n ? "mixed" : false}
      aria-label={`Select every thread in ${group.label}`}
      tabIndex={-1}
      onClick={(e) => {
        e.stopPropagation();
        selection.toggleAll(ids);
      }}
      className={`grid size-4 shrink-0 place-items-center rounded-[4px] border transition-opacity duration-150 ${n ? "border-accent bg-accent text-white" : "border-line-3 text-transparent opacity-0 group-hover/group:opacity-100"}`}
    >
      {all ? <Check className="size-3" strokeWidth={2.5} /> : <Minus className="size-3" strokeWidth={2.5} />}
    </button>
  );
}

/** Press and hold (450 ms, barely moving) on a touch screen: `fn`. */
/**
 * The click that ends a long press goes nowhere: the press selected something, the bulk bar came
 * in above the list and moved the rows, and the finger lifted over another row (it collapsed a
 * group, or selected a second thread).
 */
function swallowRelease() {
  const swallow = (ev: MouseEvent) => {
    ev.preventDefault();
    ev.stopPropagation();
  };
  window.addEventListener("click", swallow, true);
  const done = () => setTimeout(() => window.removeEventListener("click", swallow, true), 0);
  window.addEventListener("pointerup", done, { capture: true, once: true });
  window.addEventListener("pointercancel", done, { capture: true, once: true });
}

export function useLongPress(fn: () => void) {
  const t = useRef<{ timer: ReturnType<typeof setTimeout>; x: number; y: number; fired: boolean } | null>(null);
  const cancel = () => {
    if (t.current) clearTimeout(t.current.timer);
  };
  return {
    onPointerDown: (e: React.PointerEvent) => {
      if (e.pointerType === "mouse") return;
      cancel();
      const fire = () => {
        s.fired = true;
        fn();
        navigator.vibrate?.(10);
        swallowRelease();
      };
      const s = { timer: setTimeout(fire, 450), x: e.clientX, y: e.clientY, fired: false };
      t.current = s;
    },
    onPointerMove: (e: React.PointerEvent) => {
      if (t.current && Math.hypot(e.clientX - t.current.x, e.clientY - t.current.y) > 8) cancel();
    },
    onPointerUp: cancel,
    onPointerCancel: cancel,
    onContextMenu: (e: React.MouseEvent) => {
      if (t.current?.fired) e.preventDefault();
    },
    /** A long press already did its thing: the click that follows it doesn't open the thread. */
    consumed: () => {
      const fired = !!t.current?.fired;
      if (t.current) t.current.fired = false;
      return fired;
    },
  };
}

/**
 * The keyboard on a list of threads (the list, a board column): the cursor moves with j/k or
 * ↑/↓ (Shift extends the selection), Enter opens, x selects, ⌘A selects everything shown, Home and
 * End jump. Returns whether it took the key.
 */
export function listKey(e: React.KeyboardEvent, ids: readonly string[], cursor: string | null, move: (id: string) => void, open: (id: string) => void): boolean {
  if (e.altKey) return false;
  const i = cursor ? ids.indexOf(cursor) : -1;
  const go = (j: number) => {
    const id = ids[Math.max(0, Math.min(ids.length - 1, j))];
    if (!id) return;
    if (e.shiftKey) {
      if (cursor && !selection.get().ids.size) selection.toggle(cursor);
      selection.rangeTo(id, ids);
    }
    move(id);
  };
  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
    selection.toggleAll(ids.filter((id) => !id.startsWith("g:")));
    return true;
  }
  if (e.metaKey || e.ctrlKey) return false;
  switch (e.key) {
    case "j":
    case "J":
    case "ArrowDown":
      go(i + 1);
      return true;
    case "k":
    case "K":
    case "ArrowUp":
      go(i < 0 ? 0 : i - 1);
      return true;
    case "Home":
      go(0);
      return true;
    case "End":
      go(ids.length - 1);
      return true;
    case "Enter":
      if (cursor && ids.includes(cursor)) open(cursor);
      return !!cursor;
    case "x":
      if (cursor && ids.includes(cursor) && !cursor.startsWith("g:")) selection.toggle(cursor);
      return !!cursor;
  }
  return false;
}

// ── the list ──

type Col = "status" | "title" | "project" | "machine" | "agent" | "waiting" | "activity" | "changes" | "pr" | "cost";

interface ColDef {
  id: Col;
  label: string;
  sort?: SortKey;
  width: number;
  min: number;
  right?: boolean;
  /** What the column counts, after "Sort by …" in its title. */
  hint?: string;
}

const COLS: readonly ColDef[] = [
  { id: "status", label: "Status", sort: "status", width: 128, min: 88 },
  { id: "title", label: "Title", sort: "title", width: 0, min: 200 },
  { id: "project", label: "Project", sort: "project", width: 132, min: 72 },
  { id: "machine", label: "Machine", sort: "machine", width: 112, min: 64 },
  { id: "agent", label: "Agent", sort: "agent", width: 150, min: 72 },
  { id: "waiting", label: "Waiting", width: 84, min: 56 },
  { id: "activity", label: "Activity", sort: "activity", width: 84, min: 56, right: true },
  { id: "changes", label: "Changes", sort: "changes", width: 84, min: 56, right: true, hint: "The whole thread's lines, terminal edits too" },
  { id: "pr", label: "PR", width: 104, min: 56 },
  { id: "cost", label: "Cost", sort: "cost", width: 80, min: 52, right: true },
];

const ROW_H = 34;
/** Which columns give way first when the list is narrow. */
const DROP_ORDER: readonly Col[] = ["cost", "machine", "changes", "waiting", "pr", "agent", "project", "activity"];
const GROUP_H = 34;
const OVERSCAN = 8;
const WIDTHS = "roll:agents:list-widths";

function loadWidths(): Partial<Record<Col, number>> {
  try {
    const v = JSON.parse(localStorage.getItem(WIDTHS) ?? "{}") as Record<string, unknown>;
    const out: Partial<Record<Col, number>> = {};
    for (const c of COLS) if (typeof v[c.id] === "number" && c.id !== "title") out[c.id] = Math.max(c.min, Math.min(480, v[c.id] as number));
    return out;
  } catch {
    return {};
  }
}

/** Where each item starts, and the whole height (rows and group headers are fixed heights). */
function offsets(items: readonly Item[], rowH: number): { top: Float64Array; total: number } {
  const top = new Float64Array(items.length);
  let y = 0;
  for (let i = 0; i < items.length; i++) {
    top[i] = y;
    y += items[i].kind === "group" ? GROUP_H : rowH;
  }
  return { top, total: y };
}

/** The last item starting at or above `y` (binary search over `top`). */
function indexAt(top: Float64Array, y: number): number {
  let lo = 0;
  let hi = top.length - 1;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (top[mid] <= y) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

const keyOfItem = (i: Item) => (i.kind === "group" ? `g:${i.group.key}` : i.row.id);

function ThreadList({ c }: { c: Collection }) {
  const items = useMemo(() => flatten(c.groups, c.collapsed), [c.groups, c.collapsed]);
  const order = useMemo(() => orderOf(items), [items]);
  const orderRef = useRef(order);
  orderRef.current = order;
  const getOrder = useCallback(() => orderRef.current, []);
  const rowH = c.phone ? 64 : ROW_H;
  const { top, total } = useMemo(() => offsets(items, rowH), [items, rowH]);
  const scroller = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ y: 0, h: 800, w: 1400 });
  const frame = useRef(0);
  const onScroll = () => {
    if (frame.current) return;
    frame.current = requestAnimationFrame(() => {
      frame.current = 0;
      const el = scroller.current;
      if (el) setView((v) => ({ ...v, y: el.scrollTop, h: el.clientHeight }));
    });
  };
  useLayoutEffect(() => {
    const el = scroller.current;
    if (!el) return;
    const measure = () => setView({ y: el.scrollTop, h: el.clientHeight, w: el.clientWidth });
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(frame.current);
    };
  }, []);
  const [widths, setWidths] = useState<Partial<Record<Col, number>>>({});
  useEffect(() => setWidths(loadWidths()), []);
  const width = (col: ColDef) => widths[col.id] ?? col.width;
  // Narrower (a conversation open beside it), the least needed columns go first, as Linear's do.
  const cols = useMemo(() => {
    const w = (col: ColDef) => (col.id === "title" ? col.min : (widths[col.id] ?? col.width));
    let shown = [...COLS];
    for (const drop of DROP_ORDER) {
      if (28 + shown.reduce((s, col) => s + w(col), 0) <= view.w) break;
      shown = shown.filter((col) => col.id !== drop);
    }
    return shown;
  }, [widths, view.w]);
  const template = `28px ${cols.map((col) => (col.id === "title" ? `minmax(${col.min}px,1fr)` : `${width(col)}px`)).join(" ")}`;
  const minWidth = 28 + cols.reduce((s, col) => s + (col.id === "title" ? col.min : width(col)), 0);
  const colIds = useMemo(() => cols.map((col) => col.id), [cols]);

  // The keyboard cursor: an item (a row, or a group's header), kept by key so a re-sort keeps it.
  const [cursor, setCursor] = useState<string | null>(null);
  // It follows the open conversation ("Next that needs me", a click, the sidebar).
  useEffect(() => {
    if (c.open) setCursor(c.open);
  }, [c.open]);
  const index = useMemo(() => new Map(items.map((it, i) => [keyOfItem(it), i])), [items]);
  const reveal = useCallback(
    (key: string) => {
      const i = index.get(key);
      const el = scroller.current;
      if (i === undefined || !el) return;
      const y = top[i];
      const h = items[i].kind === "group" ? GROUP_H : rowH;
      const head = c.phone ? 0 : 32;
      if (y < el.scrollTop) el.scrollTop = y;
      else if (y + h > el.scrollTop + el.clientHeight - head - 96) el.scrollTop = y + h - el.clientHeight + head + 96;
    },
    [index, top, items, rowH, c.phone],
  );
  useEffect(() => {
    if (cursor) reveal(cursor);
  }, [cursor, reveal]);
  const keys = useMemo(() => items.map(keyOfItem), [items]);
  const onKey = (e: React.KeyboardEvent) => {
    const at = cursor ? index.get(cursor) : undefined;
    const item = at === undefined ? undefined : items[at];
    if (item?.kind === "group" && !e.metaKey && !e.ctrlKey) {
      if (e.key === "Enter" || e.key === " " || (e.key === "ArrowLeft" && !item.collapsed) || (e.key === "ArrowRight" && item.collapsed)) {
        e.preventDefault();
        c.toggleGroup(item.group.key);
        return;
      }
      if (e.key === "x") {
        e.preventDefault();
        selection.toggleAll(idsOf(item.group));
        return;
      }
    }
    // ← on a row goes to its group's header.
    if (item?.kind === "row" && e.key === "ArrowLeft" && !e.metaKey && index.has(`g:${item.group}`)) {
      e.preventDefault();
      setCursor(`g:${item.group}`);
      return;
    }
    // j/k walk rows and headers alike; Shift extends over rows only.
    const ids = e.shiftKey ? order : keys;
    if (listKey(e, ids, cursor, setCursor, (id) => (id.startsWith("g:") ? c.toggleGroup(id.slice(2)) : c.openThread(id)))) e.preventDefault();
  };

  const first = Math.max(0, indexAt(top, view.y) - OVERSCAN);
  const last = Math.min(items.length, indexAt(top, view.y + view.h) + OVERSCAN + 1);
  const sel = useSelection();
  const any = sel.ids.size > 0;
  const body: ReactNode[] = [];
  for (let i = first; i < last; i++) {
    const it = items[i];
    const k = keyOfItem(it);
    body.push(
      it.kind === "group" ? (
        <GroupRow key={k} item={it} y={top[i]} cursor={cursor === k} onToggle={c.toggleGroup} onCursor={setCursor} />
      ) : c.phone ? (
        <PhoneRow key={k} row={it.row} y={top[i]} c={c} order={getOrder} cursor={cursor === k} />
      ) : (
        <Row key={k} row={it.row} depth={it.depth} y={top[i]} template={template} cols={colIds} c={c} any={any} order={getOrder} cursor={cursor === k} open={c.open === it.row.id} onCursor={setCursor} />
      ),
    );
  }

  return (
    <div
      ref={scroller}
      onScroll={onScroll}
      role="grid"
      aria-label="Threads"
      aria-rowcount={order.length}
      aria-multiselectable
      tabIndex={0}
      onKeyDown={onKey}
      // The menu key or ⇧F10 on the list: the cursor row's menu.
      onContextMenu={(e) => {
        const el = cursor && !cursor.startsWith("g:") ? document.getElementById(`row-${cursor}`) : null;
        if (e.target === e.currentTarget && el) openThreadMenu(cursor!, el, e);
      }}
      onFocus={(e) => e.target === e.currentTarget && !cursor && items.length > 0 && setCursor(keyOfItem(items[0]))}
      aria-activedescendant={cursor && index.has(cursor) ? `row-${cursor}` : undefined}
      className="relative h-full overflow-auto outline-none [overflow-anchor:none]"
      data-thread-list
    >
      {!c.phone && (
        <div role="row" className="sticky top-0 z-[2] grid h-8 items-center border-b border-line bg-canvas/95 text-[11.5px] font-medium text-subtle backdrop-blur-sm" style={{ gridTemplateColumns: template, minWidth }}>
          <span />
          {cols.map((col) => (
            <HeaderCell
              key={col.id}
              col={col}
              sort={c.sort}
              onSort={c.setSort}
              onResize={
                col.id === "title"
                  ? undefined
                  : (w) =>
                      setWidths((all) => {
                        const next = { ...all, [col.id]: Math.max(col.min, Math.min(480, Math.round(w))) };
                        try {
                          localStorage.setItem(WIDTHS, JSON.stringify(next));
                        } catch {
                          // Not remembered.
                        }
                        return next;
                      })
              }
              width={width(col)}
            />
          ))}
        </div>
      )}
      {/* The prompt bar floats over the bottom of the panel: room under the last row. */}
      <div className="relative" style={{ height: total + 112, minWidth: c.phone ? undefined : minWidth }}>
        {body}
      </div>
    </div>
  );
}

function HeaderCell({ col, sort, onSort, onResize, width }: { col: ColDef; sort: Collection["sort"]; onSort: (k: SortKey) => void; onResize?: (w: number) => void; width: number }) {
  const sorted = col.sort && sort.key === col.sort;
  const Arrow = sort.dir === "asc" ? ArrowUp : ArrowDown;
  return (
    <span role="columnheader" aria-sort={sorted ? (sort.dir === "asc" ? "ascending" : "descending") : undefined} className={`relative flex h-full min-w-0 items-center px-2 ${col.right ? "justify-end" : ""}`}>
      {col.sort ? (
        <button type="button" onClick={() => onSort(col.sort!)} title={`Sort by ${col.label.toLowerCase()}${col.hint ? `. ${col.hint}` : ""}`} className={`flex min-w-0 items-center gap-1 rounded transition-colors hover:text-fg ${sorted ? "text-fg" : ""}`}>
          <span className="truncate">{col.label}</span>
          {sorted && <Arrow className="size-3 shrink-0" />}
        </button>
      ) : (
        <span className="truncate">{col.label}</span>
      )}
      {onResize && (
        <span
          role="separator"
          aria-orientation="vertical"
          aria-label={`Resize ${col.label}`}
          className="absolute top-1.5 -right-1 bottom-1.5 z-[1] w-2 cursor-col-resize after:absolute after:inset-y-0 after:left-1/2 after:w-px after:bg-line-2 after:opacity-0 after:transition-opacity hover:after:opacity-100"
          onPointerDown={(e) => {
            e.preventDefault();
            e.stopPropagation();
            const start = e.clientX;
            const el = e.currentTarget;
            el.setPointerCapture(e.pointerId);
            const move = (ev: PointerEvent) => onResize(width + ev.clientX - start);
            const up = () => {
              el.removeEventListener("pointermove", move);
              el.removeEventListener("pointerup", up);
              el.removeEventListener("pointercancel", up);
            };
            el.addEventListener("pointermove", move);
            el.addEventListener("pointerup", up);
            el.addEventListener("pointercancel", up);
          }}
          onDoubleClick={() => onResize(col.width)}
        />
      )}
    </span>
  );
}

/** Where a row sits: moved by transform, so a re-sort glides it (180 ms) and nothing else re-lays out. */
function place(y: number, reduced: boolean): CSSProperties {
  return { transform: `translateY(${y}px)`, transition: reduced ? undefined : "transform 180ms cubic-bezier(0.23,1,0.32,1)" };
}

const GroupRow = memo(function GroupRow({ item, y, cursor, onToggle, onCursor }: { item: Extract<Item, { kind: "group" }>; y: number; cursor: boolean; onToggle: (key: string) => void; onCursor: (key: string) => void }) {
  const reduced = useReducedMotion();
  const g = item.group;
  return (
    <div
      id={`row-g:${g.key}`}
      role="row"
      aria-expanded={!item.collapsed}
      aria-level={item.depth + 1}
      onClick={() => {
        onCursor(`g:${g.key}`);
        onToggle(g.key);
      }}
      style={{ ...place(y, reduced), height: GROUP_H, paddingLeft: 6 + item.depth * 20 }}
      className={`group/group absolute inset-x-0 top-0 flex cursor-pointer items-center gap-2 border-b border-line pr-3 text-[12.5px] select-none ${cursor ? "bg-[color-mix(in_oklab,var(--fg)_5%,var(--canvas))]" : "bg-[color-mix(in_oklab,var(--panel)_40%,var(--canvas))] hover:bg-[color-mix(in_oklab,var(--fg)_3%,var(--canvas))]"}`}
    >
      <GroupPick group={g} />
      <ChevronRight className={`size-3.5 shrink-0 text-subtle transition-transform duration-150 motion-reduce:transition-none ${item.collapsed ? "" : "rotate-90"}`} />
      <span className="truncate font-[560] text-fg">{g.label}</span>
      <span className="font-mono text-[11px] text-subtle tabular-nums">{g.count}</span>
    </div>
  );
});

interface RowProps {
  row: ThreadRow;
  depth: number;
  y: number;
  template: string;
  /** The columns shown (the rest gave way to a narrow list). */
  cols: readonly Col[];
  c: Collection;
  any: boolean;
  order: () => readonly ThreadId[];
  cursor: boolean;
  open: boolean;
  onCursor: (id: string) => void;
}

/** A row renders again only when what it shows changed (the stream makes new row objects on every record). */
const Row = memo(
  RowBody,
  (a, b) =>
    a.row.thread === b.row.thread &&
    a.row.last === b.row.last &&
    a.y === b.y &&
    a.template === b.template &&
    a.any === b.any &&
    a.cursor === b.cursor &&
    a.open === b.open &&
    a.depth === b.depth &&
    a.c.names === b.c.names &&
    a.c.pending.get(a.row.id) === b.c.pending.get(b.row.id) &&
    a.c.now === b.c.now &&
    a.c.openThread === b.c.openThread,
);

function RowBody({ row, y, template, cols, c, any, order, cursor, open, onCursor }: RowProps) {
  const { client } = useAgents();
  const reduced = useReducedMotion();
  const on = useSelected(row.id);
  const has = (col: Col) => cols.includes(col);
  const t = row.thread;
  const project = client.data.projects.get(t.project)?.value;
  const providers = client.data.machines.get(row.machine)?.value.info?.providers ?? [];
  const waiting = c.pending.get(row.id)?.length ?? 0;
  const info = t.model ? providers.find((p) => p.id === t.provider)?.models.find((m) => m.id === t.model) : undefined;
  const model = t.model ? (info?.short_name ?? info?.name ?? t.model) : null;
  return (
    <div
      id={`row-${row.id}`}
      role="row"
      aria-selected={on}
      data-thread={row.id}
      onClick={(e) => {
        onCursor(row.id);
        if (selectFromClick(e, row.id, order)) return;
        c.openThread(row.id);
      }}
      onContextMenu={(e) => {
        onCursor(row.id);
        onThreadContextMenu(e, row.id);
      }}
      style={{ ...place(y, reduced), height: ROW_H, gridTemplateColumns: template, ...(project ? tint(projectColor(project)) : null) }}
      className={`group/row absolute inset-x-0 top-0 grid cursor-pointer items-center border-b border-line/60 text-[13px] select-none ${
        on ? "bg-[color-mix(in_oklab,var(--accent)_8%,var(--canvas))]" : open ? "bg-[color-mix(in_oklab,var(--fg)_6%,var(--canvas))]" : cursor ? "bg-[color-mix(in_oklab,var(--fg)_4%,var(--canvas))]" : "bg-canvas hover:bg-[color-mix(in_oklab,var(--fg)_3%,var(--canvas))]"
      }`}
    >
      {/* The project's colour as the row's left edge, as on its canvas card; the cursor's in accent. */}
      <span aria-hidden className={`absolute inset-y-0 left-0 w-[2px] ${cursor ? "bg-accent" : "bg-(--tint) opacity-70"}`} />
      <span role="gridcell" className="flex justify-center">
        <Pick id={row.id} order={order} any={any} />
      </span>
      {has("status") && (
        <span role="gridcell" className="min-w-0 truncate px-2">
          <RowStatus row={row} c={c} />
        </span>
      )}
      {has("title") && (
        <span role="gridcell" className="flex min-w-0 items-baseline gap-2 px-2" title={t.activity ? `${t.title}\n${t.activity}` : t.title}>
          <span className={`truncate ${open ? "font-[560] text-fg" : "text-soft"}`}>{t.title}</span>
          {t.activity && <span className="min-w-0 flex-1 truncate text-[12px] text-subtle">{t.activity}</span>}
        </span>
      )}
      {has("project") && (
        <span role="gridcell" className="flex min-w-0 items-center gap-1.5 px-2 text-muted">
          <span aria-hidden className="size-1.5 shrink-0 rounded-full bg-(--tint)" />
          <span className="truncate">{c.names.project(t.project)}</span>
        </span>
      )}
      {has("machine") && (
        <span role="gridcell" className="truncate px-2 text-muted">
          {c.names.machine(row.machine)}
        </span>
      )}
      {has("agent") && (
        <span role="gridcell" className="truncate px-2 text-muted" title={[providerName(providers, t.provider), t.model, t.effort].filter(Boolean).join(" · ")}>
          {providerName(providers, t.provider)}
          {model && <span className="text-subtle"> · {model}</span>}
        </span>
      )}
      {has("waiting") && (
        <span role="gridcell" className="px-2">
          {waiting > 0 && <span className="rounded-full bg-accent/15 px-1.5 py-px text-[10.5px] font-medium text-accent">{waiting === 1 ? "Needs you" : `${waiting} requests`}</span>}
        </span>
      )}
      {has("activity") && (
        <span role="gridcell" className="truncate px-2 text-right font-mono text-[11.5px] text-subtle tabular-nums" title={new Date(row.activity).toLocaleString()}>
          {shortAgo(row.activity, c.now)}
        </span>
      )}
      {has("changes") && (
        <span role="gridcell" className="truncate px-2 text-right text-[11.5px]">
          <ThreadChanges thread={row.thread} last={row.last} />
        </span>
      )}
      {has("pr") && (
        <span role="gridcell" className="min-w-0 truncate px-2">
          <PrChip row={row} />
        </span>
      )}
      {has("cost") && (
        <span role="gridcell" className="truncate px-2 text-right font-mono text-[11.5px] text-subtle tabular-nums">
          {costOf(row)}
        </span>
      )}
      {/* Its menu (T3's thread menu): over the row's right end while the pointer is on it. */}
      <span className="pointer-events-none absolute top-1/2 right-1.5 -translate-y-1/2 rounded-lg border border-line-2 bg-popover p-0.5 text-muted opacity-0 shadow-pop inset-shadow-highlight transition-opacity duration-150 group-hover/row:pointer-events-auto group-hover/row:opacity-100 has-[[aria-expanded=true]]:pointer-events-auto has-[[aria-expanded=true]]:opacity-100">
        <ThreadMenuButton thread={row.id} />
      </span>
    </div>
  );
}

/** A phone's row: a two-line card; a long press (or Select) picks it, then taps pick more. */
function PhoneRow({ row, y, c, order, cursor }: { row: ThreadRow; y: number; c: Collection; order: () => readonly ThreadId[]; cursor: boolean }) {
  const { client } = useAgents();
  const on = useSelected(row.id);
  const press = useLongPress(() => {
    c.setEditing(true);
    if (!selection.get().ids.has(row.id)) selection.toggle(row.id);
  });
  const t = row.thread;
  const project = client.data.projects.get(t.project)?.value;
  const { consumed, ...handlers } = press;
  return (
    <div
      id={`row-${row.id}`}
      role="row"
      aria-selected={on}
      data-thread={row.id}
      {...handlers}
      onClick={(e) => {
        if (consumed()) return;
        if (c.editing) return selection.toggle(row.id);
        if (selectFromClick(e, row.id, order)) return;
        c.openThread(row.id);
      }}
      style={{ transform: `translateY(${y}px)`, height: 64, ...(project ? tint(projectColor(project)) : null) }}
      className={`absolute inset-x-0 top-0 flex items-center gap-3 border-b border-line/60 pr-3 pl-3.5 select-none [-webkit-touch-callout:none] ${on ? "bg-accent/[0.08]" : cursor ? "bg-fg/[0.04]" : ""}`}
    >
      <span aria-hidden className="absolute inset-y-0 left-0 w-[3px] bg-(--tint)" />
      {c.editing && (
        <span aria-hidden className={`grid size-5 shrink-0 place-items-center rounded-full border ${on ? "border-accent bg-accent text-white" : "border-line-3"}`}>
          {on && <Check className="size-3.5" strokeWidth={2.5} />}
        </span>
      )}
      <span className="min-w-0 flex-1">
        <span className="flex items-center gap-2">
          <span className="min-w-0 flex-1 truncate text-[14px] text-fg">{t.title}</span>
          <span className="shrink-0 font-mono text-[11px] text-subtle">{shortAgo(row.activity, c.now)}</span>
        </span>
        <span className="mt-1 flex items-center gap-2 text-[12px] text-muted">
          <RowStatus row={row} c={c} />
          <span className="min-w-0 truncate">
            {c.names.project(t.project)} · {c.names.machine(row.machine)}
          </span>
          <span className="ml-auto shrink-0 text-[11.5px]">
            <ThreadChanges thread={row.thread} last={row.last} />
          </span>
        </span>
      </span>
    </div>
  );
}
