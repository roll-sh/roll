"use client";

/**
 * The shelf (docs/code/agents.md, "Settling"; `lib/agents-shelf.ts`): a "Shelf" control in the
 * canvas toolbar, next to Display, opening a popover with every settled, snoozed and archived
 * conversation, grouped by project. Search narrows it, the arrows walk it, Enter opens the
 * conversation in the side panel (transcript intact) and ⌘Enter restores it (un-settle, wake or
 * unarchive) with T3's undo notice. A project's ⋯ menu, the sidebar and ⌘K open it narrowed to
 * that project. T3 has these in its sidebar's Settled shelf (`Sidebar.tsx`, `olderSettledShelf`)
 * and its archive in settings (`ArchivedThreadsPanel.tsx`); the canvas keeps them off its cards.
 */

import { AlarmClock, AlarmClockOff, Archive, ArchiveRestore, Check, Eraser, Search, Undo2, X } from "lucide-react";
import { useEffect, useId, useMemo, useRef, useState } from "react";
import { inbox, restoreOf, shortAgo, snoozeLeft, type InboxRow } from "@/lib/agents-inbox";
import { closeShelf, narrowShelf, openShelf, PARKED, PARKED_LABEL, shelfCount, shelfList, useShelf, type Parked } from "@/lib/agents-shelf";
import type { AgentsData } from "@/lib/agents-store";
import { ProjectIcon } from "./identity";
import { useInboxActions } from "./threads-sidebar";
import { AnimatedHeight } from "./animated-height";
import { Popover, PopoverPopup, PopoverTrigger, Segmented } from "./ui";
import { useAgents } from "./use-agents";

/** Rows shown before "Show more" (a long archive stays one quick list). */
export const SHELF_FIRST = 40;
const SHELF_PAGE = 60;

const RESTORE_ICON: Record<Parked, typeof Undo2> = { settled: Undo2, snoozed: AlarmClockOff, archived: ArchiveRestore };
const SHELF_ICON: Record<Parked, typeof Check> = { settled: Check, snoozed: AlarmClock, archived: Archive };

/**
 * The toolbar's control and its popover. `focused` is the project filling the canvas: opening it
 * from here narrows it to that one. Nothing on any shelf: no control.
 */
export function ShelfControl({ data, focused, phone }: { data: AgentsData; focused: string | null; phone: boolean }) {
  const shelf = useShelf();
  const groups = useMemo(() => inbox(data, Date.now()), [data]);
  const total = shelfCount(groups);
  const scoped = focused ? shelfCount(groups, focused) : total;
  // `?shelf` (or `?shelf=<project>`) opens it once: the demo's way in, and a link to someone's shelf.
  const linked = useRef(false);
  useEffect(() => {
    if (linked.current || total === 0) return;
    linked.current = true;
    const p = new URLSearchParams(window.location.search).get("shelf");
    if (p !== null) openShelf(p && data.projects.has(p) ? p : null);
  }, [total, data]);
  if (total === 0 && !shelf.open) return null;
  return (
    <Popover open={shelf.open} onOpenChange={(open) => (open ? openShelf(focused) : closeShelf())}>
      <PopoverTrigger
        render={
          <button
            type="button"
            aria-label={`Shelf: ${scoped} settled, snoozed or archived`}
            title="Settled, snoozed and archived conversations"
            className="flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-subtle transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg data-popup-open:bg-fg/[0.07] data-popup-open:text-fg"
          />
        }
      >
        <Archive className="size-3.5" strokeWidth={1.75} />
        <span className="max-sm:sr-only">Shelf</span>
        {scoped > 0 && <span className="font-mono text-[10.5px] tabular-nums">{scoped}</span>}
      </PopoverTrigger>
      <PopoverPopup
        side="bottom"
        align="end"
        sideOffset={6}
        aria-label="Shelf"
        className={`flex-col overflow-hidden text-[12.5px] ${phone ? "w-[calc(100vw-1rem)]" : "w-[400px]"}`}
      >
        {shelf.open && <ShelfPanel data={data} project={shelf.project} tab={shelf.tab} phone={phone} />}
      </PopoverPopup>
    </Popover>
  );
}

function ShelfPanel({ data, project, tab: asked, phone }: { data: AgentsData; project: string | null; tab: Parked | null; phone: boolean }) {
  const { openThread } = useAgents();
  const actions = useInboxActions();
  const [query, setQuery] = useState("");
  const [tab, setTab] = useState<Parked | null>(asked);
  const [count, setCount] = useState(SHELF_FIRST);
  const [active, setActive] = useState(0);
  const list = useMemo(() => shelfList(data, Date.now(), { project, tab, query }), [data, project, tab, query]);
  const shown = list.rows.slice(0, count);
  const at = Math.min(active, Math.max(0, shown.length - 1));
  const listId = useId();
  const field = useRef<HTMLInputElement>(null);
  const narrowed = project ? data.projects.get(project)?.value : undefined;
  // A new tab, search or project starts at the top.
  useEffect(() => setActive(0), [list.tab, query, project]);
  useEffect(() => {
    document.getElementById(`${listId}-${at}`)?.scrollIntoView?.({ block: "nearest" });
  }, [listId, at]);

  const open = (r: InboxRow) => {
    closeShelf();
    openThread(r.thread.id);
  };
  const onKey = (e: React.KeyboardEvent) => {
    const row = shown[at];
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      if (shown.length) setActive((at + (e.key === "ArrowDown" ? 1 : shown.length - 1)) % shown.length);
    } else if (e.key === "Enter" && row && !e.nativeEvent.isComposing) {
      e.preventDefault();
      if ((e.metaKey || e.ctrlKey) && !actions.readOnly) void actions.restore(row.thread);
      else open(row);
    } else if (e.key === "Escape" && query) {
      // The first Escape clears the search, the next closes.
      e.preventDefault();
      e.stopPropagation();
      setQuery("");
    }
  };

  // Section headers between the rows: the index of each row in `shown` gives its option id.
  let index = 0;
  let left = count;
  return (
    <div className="flex min-h-0 flex-col">
      <div className="flex items-center gap-2 border-b border-line px-3 py-2">
        <Search className="size-3.5 shrink-0 text-subtle" strokeWidth={1.75} />
        <input
          ref={field}
          // A phone keeps its keyboard down until the search is tapped.
          autoFocus={!phone}
          value={query}
          onChange={(e) => setQuery(e.target.value.slice(0, 200))}
          onKeyDown={onKey}
          placeholder={narrowed ? `Search ${narrowed.name}’s shelf…` : "Search the shelf…"}
          aria-label="Search the shelf"
          role="combobox"
          aria-expanded
          aria-controls={listId}
          aria-activedescendant={shown.length ? `${listId}-${at}` : undefined}
          className="min-w-0 flex-1 bg-transparent text-[13px] text-fg outline-none placeholder:text-subtle"
        />
        {narrowed && (
          <span className="flex h-6 max-w-36 shrink-0 items-center gap-1 rounded-md border border-line-2 bg-panel-2 pr-0.5 pl-1.5 text-[11.5px] text-fg">
            <ProjectIcon project={narrowed} className="size-3.5" />
            <span className="truncate">{narrowed.name}</span>
            <button
              type="button"
              onClick={() => {
                narrowShelf(null);
                field.current?.focus();
              }}
              aria-label={`Show every project's shelf, not only ${narrowed.name}`}
              title="Every project"
              className="grid size-5 shrink-0 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg"
            >
              <X className="size-3" />
            </button>
          </span>
        )}
      </div>
      <div className="px-3 pt-2">
        <Segmented
          label="Shelf"
          value={list.tab}
          onChange={(t) => {
            setTab(t);
            setCount(SHELF_FIRST);
            field.current?.focus();
          }}
          className="w-full [&>button]:flex-1 [&>button]:text-[11.5px]"
          options={PARKED.map((s) => ({ value: s, label: `${PARKED_LABEL[s]} ${list.counts[s]}` }))}
        />
      </div>
      <AnimatedHeight>
        <div id={listId} role="listbox" aria-label={`${PARKED_LABEL[list.tab]} conversations`} className="max-h-[min(60vh,440px)] overflow-y-auto px-1.5 pt-1.5 pb-1.5">
          {list.sections.length === 0 && (
            <p className="px-2 py-6 text-center text-[12px] text-subtle">
              {query ? `Nothing ${PARKED_LABEL[list.tab].toLowerCase()} matches “${query}”.` : `Nothing ${PARKED_LABEL[list.tab].toLowerCase()}${narrowed ? ` in ${narrowed.name}` : ""}.`}
            </p>
          )}
          {list.sections.map((s) => {
            if (left <= 0) return null;
            const rows = s.rows.slice(0, left);
            left -= rows.length;
            return (
              <div key={s.project.id} role="group" aria-label={s.project.name}>
                {!project && (
                  <button
                    type="button"
                    tabIndex={-1}
                    onClick={() => {
                      narrowShelf(s.project.id);
                      field.current?.focus();
                    }}
                    title={`Only ${s.project.name}`}
                    className="flex h-7 w-full items-center gap-1.5 rounded-md px-2 text-left text-[11px] font-medium text-subtle transition-colors hover:text-fg"
                  >
                    <ProjectIcon project={s.project} className="size-3.5" />
                    <span className="min-w-0 truncate">{s.project.name}</span>
                    <span className="font-mono tabular-nums">{s.rows.length}</span>
                  </button>
                )}
                {rows.map((r) => {
                  const i = index++;
                  return <ShelfRow key={r.thread.id} id={`${listId}-${i}`} row={r} active={i === at} onHover={() => setActive(i)} onOpen={() => open(r)} />;
                })}
              </div>
            );
          })}
          {list.rows.length > count && (
            <button type="button" onClick={() => setCount((n) => n + SHELF_PAGE)} className="w-full rounded-md px-2 py-1.5 text-left text-[11.5px] text-subtle transition hover:bg-fg/[0.05] hover:text-fg">
              Show {Math.min(list.rows.length - count, SHELF_PAGE)} more
            </button>
          )}
        </div>
      </AnimatedHeight>
      {!actions.readOnly && !phone && shown.length > 0 && (
        <p className="flex items-center gap-3 border-t border-line px-3 py-1.5 text-[11px] text-subtle" aria-hidden>
          <span>
            <kbd className="font-sans">↵</kbd> open
          </span>
          <span>
            <kbd className="font-sans">⌘↵</kbd> {restoreOf(list.tab)!.label.toLowerCase()}
          </span>
        </p>
      )}
    </div>
  );
}

function ShelfRow({ id, row, active, onHover, onOpen }: { id: string; row: InboxRow; active: boolean; onHover: () => void; onOpen: () => void }) {
  const actions = useInboxActions();
  const t = row.thread;
  const shelf = row.shelf as Parked;
  const r = restoreOf(shelf)!;
  const Icon = SHELF_ICON[shelf];
  const Restore = RESTORE_ICON[shelf];
  const now = Date.now();
  return (
    <div
      id={id}
      role="option"
      aria-selected={active}
      aria-label={t.title}
      onMouseMove={active ? undefined : onHover}
      onClick={onOpen}
      className={`group/s flex h-8 cursor-pointer items-center gap-2 rounded-md pr-1 pl-2 transition-colors duration-100 ${active ? "bg-fg/[0.07] text-fg" : "text-muted"}`}
    >
      <Icon className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} aria-hidden />
      <span className="min-w-0 flex-1 truncate text-[12.5px]">{t.title}</span>
      {t.worktree?.removed_at && <Eraser className="size-3 shrink-0 text-subtle" strokeWidth={1.5} aria-label="Worktree cleaned up" />}
      <span className={`shrink-0 pr-1 font-mono text-[10.5px] text-subtle ${actions.readOnly ? "" : "group-hover/s:hidden group-aria-selected/s:hidden [@media(hover:none)]:hidden"}`}>
        {shelf === "snoozed" ? snoozeLeft(row.at, now) : shortAgo(row.at, now)}
      </span>
      {!actions.readOnly && (
        <button
          type="button"
          tabIndex={-1}
          title={`${r.label} (⌘↵)`}
          aria-label={`${r.label} ${t.title}`}
          onClick={(e) => {
            e.stopPropagation();
            void actions.restore(t);
          }}
          className="hidden h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-[11.5px] text-subtle transition group-hover/s:flex group-aria-selected/s:flex hover:bg-fg/[0.07] hover:text-fg [@media(hover:none)]:flex"
        >
          <Restore className="size-3.5" strokeWidth={1.5} />
          {r.label}
        </button>
      )}
    </div>
  );
}
