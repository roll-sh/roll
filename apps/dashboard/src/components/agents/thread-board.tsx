"use client";

/**
 * The threads as Linear's board (docs/code/agents.md, "Working at scale"): a column per group
 * (status by default: Needs you · Running · Queued · Failed · Done, every column always there),
 * sub-groups as swimlanes across them. A column draws only the cards in view, so a Done column of
 * 200 scrolls like 10. Cards move as their thread's state changes: within a column they glide to
 * their new place, and one that changes column slides over from where it was (reduced motion: they
 * just appear). Dragging cards onto Done settles them (the selection, when the dragged card is in
 * it); no other column takes a drop, because the rest is what the agents are doing. Keyboard: j/k
 * or ↑/↓ in a column, h/l or ←/→ across, Enter opens, x selects.
 */

import { useReducedMotion } from "@roll/brand";
import { ChevronRight } from "lucide-react";
import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type CSSProperties } from "react";
import type { ThreadId } from "@/lib/agents-api";
import type { Group, ThreadRow } from "@/lib/agents-collection";
import { shortAgo } from "@/lib/agents-inbox";
import { projectColor } from "@/lib/agents-identity";
import { selectFromClick, selection, useSelected, useSelection } from "@/lib/agents-selection";
import { tint } from "./identity";
import { Stat } from "./shared";
import { costOf, GroupPick, listKey, Pick, PrChip, RowStatus, useLongPress, type Collection } from "./thread-list";
import { useAgents } from "./use-agents";

const CARD_H = 92;
const GAP = 8;
const STEP = CARD_H + GAP;
const COL_W = 284;
/** The narrowest a column with cards gets before the board scrolls sideways (four of them and a strip beside a conversation). */
const MIN_W = 168;
/** An empty column stays (columns never come and go) but is only its header: the room goes to the others. */
const EMPTY_W = 112;
/** The widest a column with cards grows into spare room (wider, a card reads as a strip). */
const MAX_W = 420;
/** Short of room (a conversation open beside the board), an empty column is a strip with its name down it. */
const SLIM_W = 36;
const COL_GAP = 12;
/** Cards a swimlane's cell shows before "N more". */
const CELL_FIRST = 12;

type Widths = ReadonlyMap<string, { w: number; slim: boolean }>;

/**
 * Each column's width in a board `width` wide: columns with cards share the room, from `MAX_W`
 * down to `COL_W` while everything fits, then down to `MIN_W`, then with the empty columns folded to
 * strips; only past that does the board scroll sideways. A phone shows a column at a time (snap).
 */
function columnWidths(columns: readonly Group[], width: number, phone: boolean): Widths {
  const full = (g: Group) => (g.count ? COL_W : EMPTY_W);
  const out = new Map<string, { w: number; slim: boolean }>();
  const n = columns.filter((g) => g.count).length;
  const empty = columns.length - n;
  const room = width - 24 - COL_GAP * Math.max(0, columns.length - 1);
  const fits = (emptyW: number, w: number) => n * w + empty * emptyW <= room;
  if (phone || !width || n === 0) {
    for (const g of columns) out.set(g.key, { w: full(g), slim: false });
    return out;
  }
  if (fits(EMPTY_W, COL_W)) {
    // Room to spare (bea: Done alone beside four empty columns left 650 px blank): the columns with cards take it.
    const w = Math.floor(Math.min(MAX_W, (room - empty * EMPTY_W) / n));
    for (const g of columns) out.set(g.key, { w: g.count ? w : EMPTY_W, slim: false });
    return out;
  }
  const slim = !fits(EMPTY_W, MIN_W);
  const emptyW = slim ? SLIM_W : EMPTY_W;
  const w = Math.floor(Math.max(MIN_W, Math.min(COL_W, (room - empty * emptyW) / n)));
  for (const g of columns) out.set(g.key, g.count ? { w, slim: false } : { w: emptyW, slim });
  return out;
}
const widthIn = (widths: Widths, g: Group) => widths.get(g.key)?.w ?? (g.count ? COL_W : EMPTY_W);

// A card that changes column slides over from where it was. Only those cards are measured: the
// board notes which threads changed column as it renders (the old card is still on the page then)
// and reads just their rects; a card scrolling in or out reads nothing.
const mounted = new Map<ThreadId, HTMLElement>();
const columnOf = new Map<ThreadId, string>();
const movedFrom = new Map<ThreadId, { x: number; y: number }>();

function noteMoves(columns: readonly Group[]) {
  for (const g of columns)
    for (const r of g.rows) {
      const was = columnOf.get(r.id);
      if (was !== undefined && was !== g.key) {
        const el = mounted.get(r.id);
        if (el?.isConnected) {
          const b = el.getBoundingClientRect();
          movedFrom.set(r.id, { x: b.left, y: b.top });
        }
      }
      columnOf.set(r.id, g.key);
    }
}

export function ThreadBoard({ c }: { c: Collection }) {
  const lanes = c.groups.some((g) => g.groups?.length);
  const [cursor, setCursor] = useState<ThreadId | null>(null);
  useEffect(() => {
    if (c.open) setCursor(c.open);
  }, [c.open]);
  const columns = c.groups;
  // The board's own width (a conversation open beside it takes half the window): columns fit it.
  const root = useRef<HTMLDivElement>(null);
  const [width, setWidth] = useState(0);
  useLayoutEffect(() => {
    const el = root.current;
    if (!el) return;
    setWidth(el.clientWidth);
    const ro = new ResizeObserver(() => setWidth(el.clientWidth));
    ro.observe(el);
    return () => ro.disconnect();
  }, []);
  const widths = useMemo(() => columnWidths(columns, width, c.phone), [columns, width, c.phone]);
  // Before this render's cards replace the last ones.
  const noted = useRef<readonly Group[] | null>(null);
  if (noted.current !== columns) {
    noted.current = columns;
    if (!lanes) noteMoves(columns);
  }
  // Columns as lists of ids (lanes flattened), for the keyboard.
  const cols = useMemo(() => columns.map((g) => g.rows.map((r) => r.id)), [columns]);
  const onKey = (e: React.KeyboardEvent) => {
    const ci = Math.max(0, cols.findIndex((ids) => cursor !== null && ids.includes(cursor)));
    const across = e.key === "ArrowLeft" || e.key === "h" ? -1 : e.key === "ArrowRight" || e.key === "l" ? 1 : 0;
    if (across && !e.metaKey && !e.ctrlKey && !e.altKey) {
      e.preventDefault();
      const at = cursor ? cols[ci].indexOf(cursor) : 0;
      for (let j = ci + across; j >= 0 && j < cols.length; j += across) {
        if (!cols[j].length) continue;
        setCursor(cols[j][Math.min(Math.max(0, at), cols[j].length - 1)]);
        return;
      }
      return;
    }
    const ids = cols[ci] ?? [];
    if (listKey(e, ids, cursor && ids.includes(cursor) ? cursor : null, setCursor, c.openThread)) e.preventDefault();
    else if (!cursor && (e.key === "j" || e.key === "ArrowDown")) {
      const first = cols.find((x) => x.length)?.[0];
      if (first) setCursor(first);
    }
  };
  return (
    <div ref={root} role="grid" aria-label="Threads board" tabIndex={0} onKeyDown={onKey} className="h-full overflow-auto outline-none max-sm:snap-x max-sm:snap-mandatory max-sm:scroll-px-3" data-thread-board>
      {lanes ? <Lanes c={c} widths={widths} cursor={cursor} setCursor={setCursor} /> : <Columns c={c} columns={columns} widths={widths} cursor={cursor} setCursor={setCursor} />}
    </div>
  );
}

/** Whether a column takes dropped cards: only Done, by status (dropping there settles them). */
const takesDrop = (c: Collection, g: Group) => c.by === "status" && g.value === "done";

function Columns({ c, columns, widths, cursor, setCursor }: { c: Collection; columns: Group[]; widths: Widths; cursor: ThreadId | null; setCursor: (id: ThreadId) => void }) {
  return (
    <div className="flex h-full min-w-max gap-3 px-3 pt-3">
      {columns.map((g) =>
        widths.get(g.key)?.slim ? <SlimColumn key={g.key} group={g} /> : <Column key={g.key} c={c} group={g} width={widthIn(widths, g)} cursor={cursor} setCursor={setCursor} />,
      )}
    </div>
  );
}

/** An empty column short of room: its name and count down a strip. */
function SlimColumn({ group }: { group: Group }) {
  return (
    <section aria-label={`${group.label}, ${group.count}`} style={{ width: SLIM_W }} className="flex shrink-0 flex-col items-center gap-2 pt-2 text-[12.5px]" title={`${group.label}: none`}>
      <span className="font-mono text-[11px] text-subtle tabular-nums">{group.count}</span>
      <span className="font-[560] whitespace-nowrap text-muted [writing-mode:vertical-rl]">{group.label}</span>
    </section>
  );
}

function ColumnHead({ group, drop }: { group: Group; drop: boolean }) {
  return (
    <div className="group/group flex h-8 shrink-0 items-center gap-2 px-1.5 text-[12.5px]">
      {group.count > 0 && <GroupPick group={group} />}
      <span className="truncate font-[560] text-fg">{group.label}</span>
      <span className="font-mono text-[11px] text-subtle tabular-nums">{group.count}</span>
      {drop && <span className="ml-auto text-[11px] text-subtle">Drop to settle</span>}
    </div>
  );
}

function useDrop(c: Collection, group: Group) {
  const [over, setOver] = useState(false);
  const ok = takesDrop(c, group);
  return {
    over,
    props: ok
      ? {
          onDragOver: (e: React.DragEvent) => {
            if (!e.dataTransfer.types.includes(DRAG_TYPE)) return;
            e.preventDefault();
            e.dataTransfer.dropEffect = "move";
            if (!over) setOver(true);
          },
          onDragLeave: (e: React.DragEvent) => {
            if (!(e.currentTarget as HTMLElement).contains(e.relatedTarget as Node | null)) setOver(false);
          },
          onDrop: (e: React.DragEvent) => {
            e.preventDefault();
            setOver(false);
            const ids = e.dataTransfer.getData(DRAG_TYPE).split(",").filter(Boolean);
            if (ids.length) c.settle(ids);
          },
        }
      : {},
  };
}

const DRAG_TYPE = "text/x-roll-threads";

function Column({ c, group, width, cursor, setCursor }: { c: Collection; group: Group; width: number; cursor: ThreadId | null; setCursor: (id: ThreadId) => void }) {
  const body = useRef<HTMLDivElement>(null);
  const [view, setView] = useState({ y: 0, h: 800 });
  const frame = useRef(0);
  useLayoutEffect(() => {
    const el = body.current;
    if (!el) return;
    setView({ y: el.scrollTop, h: el.clientHeight });
    const ro = new ResizeObserver(() => setView({ y: el.scrollTop, h: el.clientHeight }));
    ro.observe(el);
    return () => {
      ro.disconnect();
      cancelAnimationFrame(frame.current);
    };
  }, []);
  // The cursor's card scrolls into its column's view.
  const at = cursor ? group.rows.findIndex((r) => r.id === cursor) : -1;
  useEffect(() => {
    const el = body.current;
    if (!el || at < 0) return;
    const y = at * STEP;
    if (y < el.scrollTop) el.scrollTop = y;
    else if (y + CARD_H > el.scrollTop + el.clientHeight - 96) el.scrollTop = y + CARD_H - el.clientHeight + 96;
  }, [at]);
  const drop = useDrop(c, group);
  const first = Math.max(0, Math.floor(view.y / STEP) - 3);
  const last = Math.min(group.rows.length, Math.ceil((view.y + view.h) / STEP) + 3);
  const order = useCallback(() => group.rows.map((r) => r.id), [group.rows]);
  const sel = useSelection();
  const cards = [];
  for (let i = first; i < last; i++) {
    const r = group.rows[i];
    cards.push(<BoardCard key={r.id} row={r} c={c} column={group.key} y={i * STEP} order={order} any={sel.ids.size > 0} cursor={cursor === r.id} setCursor={setCursor} />);
  }
  return (
    <section aria-label={`${group.label}, ${group.count}`} style={{ width }} className="@container/column flex min-h-0 shrink-0 snap-start flex-col" {...drop.props}>
      <ColumnHead group={group} drop={takesDrop(c, group)} />
      <div
        ref={body}
        onScroll={() => {
          if (frame.current) return;
          frame.current = requestAnimationFrame(() => {
            frame.current = 0;
            const el = body.current;
            if (el) setView({ y: el.scrollTop, h: el.clientHeight });
          });
        }}
        className={`relative min-h-0 flex-1 overflow-y-auto rounded-lg transition-colors duration-150 [overflow-anchor:none] [scrollbar-width:thin] ${drop.over ? "bg-accent/[0.06] outline outline-1 -outline-offset-1 outline-accent/40" : ""}`}
      >
        <div className="relative" style={{ height: group.rows.length * STEP + 112 }}>
          {cards}
        </div>
      </div>
    </section>
  );
}

/** Sub-grouped: a lane per sub-group, folding, each a row of cells under the columns. */
function Lanes({ c, widths, cursor, setCursor }: { c: Collection; widths: Widths; cursor: ThreadId | null; setCursor: (id: ThreadId) => void }) {
  const columns = c.groups;
  // The lanes: every sub-group any column has, in the first order they appear.
  const lanes = useMemo(() => {
    const seen = new Map<string, { value: string; label: string; count: number }>();
    for (const col of columns) for (const s of col.groups ?? []) seen.set(s.value, { value: s.value, label: s.label, count: (seen.get(s.value)?.count ?? 0) + s.count });
    return [...seen.values()].sort((a, b) => a.label.localeCompare(b.label));
  }, [columns]);
  return (
    <div className="min-w-max px-3 pt-3 pb-28">
      <div className="sticky top-0 z-[2] flex gap-3 bg-canvas pb-1">
        {columns.map((g) => (
          <div key={g.key} style={{ width: widthIn(widths, g) }}>
            {widths.get(g.key)?.slim ? <SlimColumn group={g} /> : <ColumnHead group={g} drop={false} />}
          </div>
        ))}
      </div>
      {lanes.map((lane) => {
        const key = `lane:${lane.value}`;
        const shut = c.collapsed.has(key);
        return (
          <div key={lane.value} className="border-t border-line pt-1.5 pb-2">
            <button type="button" onClick={() => c.toggleGroup(key)} aria-expanded={!shut} className="flex h-7 items-center gap-1.5 rounded px-1 text-[12.5px] text-muted hover:text-fg">
              <ChevronRight className={`size-3.5 transition-transform duration-150 motion-reduce:transition-none ${shut ? "" : "rotate-90"}`} />
              <span className="font-[560] text-fg">{lane.label}</span>
              <span className="font-mono text-[11px] text-subtle tabular-nums">{lane.count}</span>
            </button>
            {!shut && (
              <div className="flex gap-3">
                {columns.map((col) => (
                  <Cell key={col.key} c={c} group={col.groups?.find((s) => s.value === lane.value)} column={col} width={widthIn(widths, col)} cursor={cursor} setCursor={setCursor} />
                ))}
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

function Cell({ c, group, column, width, cursor, setCursor }: { c: Collection; group: Group | undefined; column: Group; width: number; cursor: ThreadId | null; setCursor: (id: ThreadId) => void }) {
  const [all, setAll] = useState(false);
  const drop = useDrop(c, column);
  const rows = group?.rows ?? [];
  const shown = all ? rows : rows.slice(0, CELL_FIRST);
  const order = useCallback(() => rows.map((r) => r.id), [rows]);
  const sel = useSelection();
  return (
    <div style={{ width }} className={`@container/column flex min-h-12 flex-col gap-2 rounded-lg transition-colors duration-150 ${drop.over ? "bg-accent/[0.06] outline outline-1 -outline-offset-1 outline-accent/40" : ""}`} {...drop.props}>
      {shown.map((r) => (
        <div key={r.id} className="relative" style={{ height: CARD_H }}>
          <BoardCard row={r} c={c} column={column.key} y={0} order={order} any={sel.ids.size > 0} cursor={cursor === r.id} setCursor={setCursor} />
        </div>
      ))}
      {rows.length > shown.length && (
        <button type="button" onClick={() => setAll(true)} className="h-7 rounded-md text-[12px] text-subtle hover:bg-fg/[0.04] hover:text-fg">
          {rows.length - shown.length} more
        </button>
      )}
    </div>
  );
}

interface CardProps {
  row: ThreadRow;
  c: Collection;
  column: string;
  y: number;
  order: () => readonly ThreadId[];
  any: boolean;
  cursor: boolean;
  setCursor: (id: ThreadId) => void;
}

const BoardCard = memo(
  BoardCardBody,
  (a, b) => a.row.thread === b.row.thread && a.row.last === b.row.last && a.column === b.column && a.y === b.y && a.any === b.any && a.cursor === b.cursor && a.c.open === b.c.open && a.c.now === b.c.now && a.c.names === b.c.names && a.c.pending.get(a.row.id) === b.c.pending.get(b.row.id) && a.c.editing === b.c.editing,
);

function BoardCardBody({ row, c, column, y, order, any, cursor, setCursor }: CardProps) {
  const { client } = useAgents();
  const reduced = useReducedMotion();
  const on = useSelected(row.id);
  const el = useRef<HTMLDivElement>(null);
  const t = row.thread;
  const project = client.data.projects.get(t.project)?.value;
  const press = useLongPress(() => {
    c.setEditing(true);
    if (!selection.get().ids.has(row.id)) selection.toggle(row.id);
  });
  const { consumed, ...touch } = press;
  // A card that changed column slides over from where it was (measured only then).
  useLayoutEffect(() => {
    const node = el.current;
    if (!node) return;
    mounted.set(row.id, node);
    const from = movedFrom.get(row.id);
    movedFrom.delete(row.id);
    if (from && !reduced && typeof node.animate === "function") {
      const r = node.getBoundingClientRect();
      node.animate([{ transform: `translate(${from.x - r.left}px, ${from.y - r.top}px)`, opacity: 0.6 }, { transform: "translate(0,0)", opacity: 1 }], { duration: 260, easing: "cubic-bezier(0.23,1,0.32,1)", composite: "add" });
    }
    return () => {
      if (mounted.get(row.id) === node) mounted.delete(row.id);
    };
  }, [row.id, column, reduced]);
  const style: CSSProperties = {
    transform: `translateY(${y}px)`,
    transition: reduced ? undefined : "transform 200ms cubic-bezier(0.23,1,0.32,1)",
    height: CARD_H,
    ...(project ? tint(projectColor(project)) : null),
  };
  const open = c.open === row.id;
  return (
    <div
      ref={el}
      role="row"
      aria-selected={on}
      aria-label={t.title}
      data-thread={row.id}
      draggable={!c.phone}
      onDragStart={(e) => {
        const ids = on ? [...selection.get().ids] : [row.id];
        e.dataTransfer.setData(DRAG_TYPE, ids.join(","));
        e.dataTransfer.effectAllowed = "move";
      }}
      {...touch}
      onClick={(e) => {
        setCursor(row.id);
        if (consumed()) return;
        if (c.editing) return selection.toggle(row.id);
        if (selectFromClick(e, row.id, order)) return;
        c.openThread(row.id);
      }}
      style={style}
      className={`group/row absolute inset-x-0 top-0 flex cursor-pointer flex-col gap-1.5 overflow-hidden rounded-[10px] border bg-panel py-2 pr-2.5 pl-3.5 text-[12.5px] shadow-card inset-shadow-highlight select-none [-webkit-touch-callout:none] ${
        on ? "border-accent/55 bg-[color-mix(in_oklab,var(--accent)_7%,var(--panel))]" : cursor ? "border-line-3" : open ? "border-line-3" : "border-line-2 hover:border-line-3"
      }`}
    >
      {/* The project's colour, the card's full height (as on the canvas). */}
      <span aria-hidden className="pointer-events-none absolute inset-y-0 left-0 w-[3px] bg-(--tint)" />
      <span className="flex items-center gap-2">
        <RowStatus row={row} c={c} />
        <span className="ml-auto flex items-center gap-2">
          <span className="font-mono text-[10.5px] text-subtle">{shortAgo(row.activity, c.now)}</span>
          <Pick id={row.id} order={order} any={any || c.editing} />
        </span>
      </span>
      <span className={`line-clamp-2 leading-[1.35] @max-[13.75rem]/column:line-clamp-1 ${open ? "font-[560] text-fg" : "text-soft"}`}>{t.title}</span>
      <span className="mt-auto flex min-w-0 items-center gap-2 text-[11.5px] text-muted">
        <span aria-hidden className="size-1.5 shrink-0 rounded-full bg-(--tint)" />
        <span className="min-w-0 truncate">
          {c.names.project(t.project)} · {c.names.machine(row.machine)}
        </span>
        <span className="ml-auto flex shrink-0 items-center gap-2">
          <PrChip row={row} />
          <Stat stat={row.last?.status === "rolled_back" ? null : row.last?.stat} className="text-[11px]" />
          <span className="font-mono text-[10.5px] text-subtle">{costOf(row)}</span>
        </span>
      </span>
    </div>
  );
}
