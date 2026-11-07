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

import { AlarmClock, AlarmClockOff, Archive, ArchiveRestore, Check, Search, Undo2, X } from "lucide-react";
import { AnimatePresence } from "motion/react";
import { useEffect, useId, useMemo, useRef, useState, type ReactNode } from "react";
import { inbox, restoreOf, shortAgo, snoozeLeft, type InboxRow } from "@/lib/agents-inbox";
import { closeShelf, narrowShelf, openShelf, PARKED, PARKED_LABEL, shelfCount, shelfList, useShelf, type Parked } from "@/lib/agents-shelf";
import { selectFromClick, selection, useSelection } from "@/lib/agents-selection";
import type { AgentsData } from "@/lib/agents-store";
import { ProjectIcon } from "./identity";
import { useInboxActions } from "./threads-sidebar";
import { AnimatedHeight } from "./animated-height";
import { useFold } from "./list-motion";
import { Popover, PopoverPopup, PopoverTrigger, Segmented } from "./ui";
import { useAgents } from "./use-agents";

/** Rows shown before "Show more" (a long archive stays one quick list). */
export const SHELF_FIRST = 40;
const SHELF_PAGE = 60;

const RESTORE_ICON: Record<Parked, typeof Undo2> = { settled: Undo2, snoozed: AlarmClockOff, archived: ArchiveRestore };
const SHELF_ICON: Record<Parked, typeof Check> = { settled: Check, snoozed: AlarmClock, archived: Archive };

/**
 * The toolbar's control and its popover. `focused` is the project filling the canvas: opening it
 * from here narrows it to that one. An empty shelf keeps its control (and says what goes there).
 */
export function ShelfControl({ data, focused, phone }: { data: AgentsData; focused: string | null; phone: boolean }) {
  const shelf = useShelf();
  const groups = useMemo(() => inbox(data, Date.now()), [data]);
  const total = shelfCount(groups);
  const scoped = focused ? shelfCount(groups, focused) : total;
  // `?shelf` (or `?shelf=<project>`) opens it once: the demo's way in, and a link to someone's shelf.
  const linked = useRef(false);
  useEffect(() => {
    if (linked.current) return;
    linked.current = true;
    const p = new URLSearchParams(window.location.search).get("shelf");
    if (p !== null) openShelf(p && data.projects.has(p) ? p : null);
  }, [data]);
  return (
    <Popover open={shelf.open} onOpenChange={(open) => (open ? openShelf(focused) : closeShelf())}>
      <PopoverTrigger
        render={
          <button
            type="button"
            aria-label={`Shelf: ${scoped} settled, snoozed or archived`}
            title="Settled, snoozed and archived conversations (⇧S)"
            className="flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-subtle transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg data-popup-open:bg-fg/[0.07] data-popup-open:text-fg"
          />
        }
      >
        <Archive className="size-3.5" strokeWidth={1.75} />
        <span className="max-sm:sr-only @max-[520px]/views:sr-only">Shelf</span>
        {scoped > 0 && <span className="font-mono text-[10.5px] tabular-nums @max-[520px]/views:hidden">{scoped}</span>}
      </PopoverTrigger>
      <PopoverPopup
        side="bottom"
        align="end"
        sideOffset={6}
        aria-label="Shelf"
        className={`flex-col overflow-hidden text-[12.5px] ${phone ? "w-[calc(100vw-1rem)]" : "w-[400px]"}`}
      >
        {/* Kept while the popup fades out (Base UI unmounts it after): it closed to an empty box. */}
        <ShelfPanel data={data} project={shelf.project} tab={shelf.tab} phone={phone} />
      </PopoverPopup>
    </Popover>
  );
}

function ShelfPanel({ data, project, tab: asked, phone }: { data: AgentsData; project: string | null; tab: Parked | null; phone: boolean }) {
  const { openThread, bulk } = useAgents();
  const actions = useInboxActions();
  const [query, setQuery] = useState("");
  const [tab, setTab] = useState<Parked | null>(asked);
  const [count, setCount] = useState(SHELF_FIRST);
  const [active, setActive] = useState(0);
  // Picking: the rows' checkboxes stay out (a phone has no hover to show them).
  const [picking, setPicking] = useState(false);
  const sel = useSelection();
  const list = useMemo(() => shelfList(data, Date.now(), { project, tab, query }), [data, project, tab, query]);
  const shown = list.rows.slice(0, count);
  const at = Math.min(active, Math.max(0, shown.length - 1));
  const listId = useId();
  const field = useRef<HTMLInputElement>(null);
  const narrowed = project ? data.projects.get(project)?.value : undefined;
  const order = useMemo(() => list.rows.map((r) => r.thread.id), [list.rows]);
  // What's picked here: the shared selection's threads on this tab (others stay picked elsewhere).
  const picked = useMemo(() => order.filter((id) => sel.ids.has(id)), [order, sel.ids]);
  const canPick = !actions.readOnly && !!bulk;
  const empty = PARKED.every((s) => list.counts[s] === 0);
  // A new tab, search or project starts at the top.
  useEffect(() => setActive(0), [list.tab, query, project]);
  // The tab it opened on stays when its last row goes back (it jumped to an empty Settled).
  useEffect(() => {
    if (tab === null && list.rows.length > 0) setTab(list.tab);
  }, [tab, list.tab, list.rows.length]);
  useEffect(() => {
    document.getElementById(`${listId}-${at}`)?.scrollIntoView?.({ block: "nearest" });
  }, [listId, at]);
  useEffect(() => {
    if (!picked.length && !list.rows.length) setPicking(false);
  }, [picked.length, list.rows.length]);

  const open = (r: InboxRow) => {
    closeShelf();
    openThread(r.thread.id);
  };
  const restorePicked = () => {
    if (!bulk || !picked.length) return;
    bulk({ kind: "restore" }, picked);
    setPicking(false);
  };
  const onKey = (e: React.KeyboardEvent) => {
    const row = shown[at];
    if (e.key === "ArrowDown" || e.key === "ArrowUp") {
      e.preventDefault();
      // Down from the last row shown shows the next page (it wrapped to the top at row 40, so
      // "Show more" was only reachable by mouse); once everything shows, it goes round.
      if (e.key === "ArrowDown" && at === shown.length - 1 && shown.length < list.rows.length) {
        setCount((n) => n + SHELF_PAGE);
        setActive(at + 1);
      } else if (shown.length) setActive((at + (e.key === "ArrowDown" ? 1 : shown.length - 1)) % shown.length);
    } else if (e.key === "Enter" && row && !e.nativeEvent.isComposing) {
      e.preventDefault();
      if ((e.metaKey || e.ctrlKey) && !actions.readOnly) {
        // ⌘↵ restores what's picked, or the row it's on.
        if (picked.length) restorePicked();
        else void actions.restore(row.thread);
      } else open(row);
    } else if (e.key === " " && e.shiftKey && row && canPick) {
      // ⇧Space picks the row it's on (plain Space types in the search).
      e.preventDefault();
      selection.toggle(row.thread.id);
    } else if (e.key === "Escape" && query) {
      // The first Escape clears the search, then the picks, then it closes.
      e.preventDefault();
      e.stopPropagation();
      setQuery("");
    } else if (e.key === "Escape" && (picked.length || picking)) {
      e.preventDefault();
      e.stopPropagation();
      clearPicked(picked);
      setPicking(false);
    }
  };
  const pick = (e: React.MouseEvent, id: string) => {
    if (e.shiftKey) selection.rangeTo(id, order);
    else selection.toggle(id);
  };

  // Section headers between the rows: the index of each row in `shown` gives its option id.
  let index = 0;
  let left = count;
  const showBoxes = canPick && (picking || picked.length > 0);
  return (
    <div className="flex min-h-0 flex-col">
      {/* Nothing anywhere on the shelf: no search, only what goes here. */}
      <div className={`flex items-center gap-2 border-b border-line px-3 py-2 ${empty && !query && !narrowed ? "hidden" : ""}`}>
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
      {empty && !query && !narrowed ? (
        <div className="flex flex-col items-center gap-1.5 px-6 py-8 text-center">
          <Archive className="mb-1 size-4 text-subtle" strokeWidth={1.5} aria-hidden />
          <p className="text-[12.5px] text-fg">The shelf is empty</p>
          <p className="max-w-64 text-[12px] leading-relaxed text-subtle">Conversations you settle, snooze or archive wait here, with their transcripts, until you bring them back.</p>
        </div>
      ) : (
        <>
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
            <div id={listId} role="listbox" aria-multiselectable={canPick || undefined} aria-label={`${PARKED_LABEL[list.tab]} conversations`} className="max-h-[min(60vh,440px)] overflow-y-auto px-1.5 pt-1.5 pb-1.5">
              {list.sections.length === 0 && (
                <p className="px-2 py-6 text-center text-[12px] text-subtle">
                  {query ? `Nothing ${PARKED_LABEL[list.tab].toLowerCase()} matches “${query}”.` : `Nothing ${PARKED_LABEL[list.tab].toLowerCase()}${narrowed ? ` in ${narrowed.name}` : ""}.`}
                </p>
              )}
              {/* A row restored (or woken) folds out, and its project with its last row; a tab or a
                  search shows its rows at once (keyed by them). */}
              <AnimatePresence key={`${list.tab}|${query}`} initial={false}>
                {list.sections.map((s) => {
                  if (left <= 0) return null;
                  const rows = s.rows.slice(0, left);
                  left -= rows.length;
                  return (
                    <ShelfSection key={s.project.id} name={s.project.name}>
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
                      <AnimatePresence initial={false}>
                        {rows.map((r) => {
                          const i = index++;
                          return (
                            <ShelfRow
                              key={r.thread.id}
                              id={`${listId}-${i}`}
                              row={r}
                              active={i === at}
                              picked={sel.ids.has(r.thread.id)}
                              boxes={showBoxes}
                              onPick={canPick ? (e) => pick(e, r.thread.id) : undefined}
                              onHover={() => setActive(i)}
                              onOpen={(e) => {
                                // ⌘/Shift-click picks, as on the list and the canvas; a click while picking picks too.
                                if (canPick && selectFromClick(e, r.thread.id, () => order)) return;
                                if (canPick && showBoxes) selection.toggle(r.thread.id);
                                else open(r);
                              }}
                            />
                          );
                        })}
                      </AnimatePresence>
                    </ShelfSection>
                  );
                })}
              </AnimatePresence>
              {list.rows.length > count && (
                <button type="button" onClick={() => setCount((n) => n + SHELF_PAGE)} className="w-full rounded-md px-2 py-1.5 text-left text-[11.5px] text-subtle transition hover:bg-fg/[0.05] hover:text-fg">
                  Show {Math.min(list.rows.length - count, SHELF_PAGE)} more
                </button>
              )}
            </div>
          </AnimatedHeight>
          {canPick && showBoxes ? (
            <div role="toolbar" aria-label={`${picked.length} picked`} className="flex h-9 items-center gap-1 border-t border-line px-2 text-[12px]">
              <span className="px-1 text-muted">
                <span className="font-mono tabular-nums">{picked.length}</span> picked
              </span>
              {picked.length < list.rows.length && (
                <button type="button" onClick={() => selection.toggleAll(order)} className="rounded-md px-1.5 py-1 text-subtle transition hover:bg-fg/[0.06] hover:text-fg">
                  Pick all {list.rows.length}
                </button>
              )}
              <button
                type="button"
                onClick={() => {
                  clearPicked(picked);
                  setPicking(false);
                }}
                className="rounded-md px-1.5 py-1 text-subtle transition hover:bg-fg/[0.06] hover:text-fg"
              >
                {picked.length ? "Clear" : "Cancel"}
              </button>
              <button
                type="button"
                disabled={!picked.length}
                onClick={restorePicked}
                title={`${restoreOf(list.tab)!.label} each picked conversation (⌘↵)`}
                className="ml-auto flex h-7 items-center gap-1.5 rounded-md bg-fg px-2.5 font-medium text-bg transition hover:opacity-85 disabled:opacity-40"
              >
                {(() => {
                  const R = RESTORE_ICON[list.tab];
                  return <R className="size-3.5" strokeWidth={1.75} />;
                })()}
                {restoreOf(list.tab)!.label}
                {picked.length > 0 && <span className="font-mono tabular-nums">{picked.length}</span>}
              </button>
            </div>
          ) : (
            !actions.readOnly &&
            shown.length > 0 && (
              <div className="flex items-center gap-3 border-t border-line py-1 pr-1.5 pl-3 text-[11px] text-subtle">
                {!phone && (
                  <>
                    <span aria-hidden>
                      <kbd className="font-sans">↵</kbd> open
                    </span>
                    <span aria-hidden>
                      <kbd className="font-sans">⌘↵</kbd> {restoreOf(list.tab)!.label.toLowerCase()}
                    </span>
                  </>
                )}
                {canPick && (
                  <button type="button" onClick={() => setPicking(true)} title="Pick several to restore at once (⇧-click a range)" className="ml-auto rounded-md px-1.5 py-1 transition hover:bg-fg/[0.06] hover:text-fg">
                    Select
                  </button>
                )}
              </div>
            )
          )}
        </>
      )}
    </div>
  );
}

/** Lets go of these (the shelf's picks), leaving what's picked elsewhere. */
function clearPicked(ids: readonly string[]) {
  const s = selection.get();
  selection.replace([...s.ids].filter((id) => !ids.includes(id)));
}

/** A project's rows on the shelf: folds out with its last row. */
function ShelfSection({ name, children }: { name: string; children: ReactNode }) {
  const fold = useFold<HTMLDivElement>();
  return (
    <div ref={fold.ref} inert={fold.leaving} role="group" aria-label={name}>
      {children}
    </div>
  );
}

function ShelfRow({
  id,
  row,
  active,
  picked,
  boxes,
  onPick,
  onHover,
  onOpen,
}: {
  id: string;
  row: InboxRow;
  active: boolean;
  picked: boolean;
  /** Checkboxes shown on every row (picking); otherwise only on hover. */
  boxes: boolean;
  onPick?: (e: React.MouseEvent) => void;
  onHover: () => void;
  onOpen: (e: React.MouseEvent) => void;
}) {
  const actions = useInboxActions();
  const t = row.thread;
  const shelf = row.shelf as Parked;
  const r = restoreOf(shelf)!;
  const Icon = SHELF_ICON[shelf];
  const Restore = RESTORE_ICON[shelf];
  const now = Date.now();
  const fold = useFold<HTMLDivElement>();
  return (
    <div
      ref={fold.ref}
      inert={fold.leaving}
      id={id}
      role="option"
      aria-selected={active}
      aria-checked={onPick ? picked : undefined}
      aria-label={t.title}
      onMouseMove={active ? undefined : onHover}
      onClick={onOpen}
      className={`group/s flex h-8 cursor-pointer items-center gap-2 rounded-md pr-1 pl-2 transition-colors duration-100 ${picked ? "bg-accent/[0.08] text-fg" : active ? "bg-fg/[0.07] text-fg" : "text-muted"}`}
    >
      {onPick ? (
        <button
          type="button"
          tabIndex={-1}
          role="checkbox"
          aria-checked={picked}
          aria-label={`Pick ${t.title}`}
          onClick={(e) => {
            e.stopPropagation();
            onPick(e);
          }}
          className="relative grid size-3.5 shrink-0 place-items-center"
        >
          <Icon className={`size-3.5 text-subtle ${boxes || picked ? "hidden" : "group-hover/s:hidden"}`} strokeWidth={1.5} aria-hidden />
          <span
            aria-hidden
            className={`size-3.5 place-items-center rounded-[4px] border ${picked ? "grid border-accent bg-accent text-white" : `border-line-2 ${boxes ? "grid" : "hidden group-hover/s:grid"}`}`}
          >
            {picked && <Check className="size-2.5" strokeWidth={2.5} />}
          </span>
        </button>
      ) : (
        <Icon className="size-3.5 shrink-0 text-subtle" strokeWidth={1.5} aria-hidden />
      )}
      <span className="min-w-0 flex-1 truncate text-[13px]">{t.title}</span>
      {t.worktree?.removed_at && <span className="shrink-0 text-[11px] text-subtle" title="Its worktree was cleaned up; restoring it makes a new one">cleaned up</span>}
      <span className={`shrink-0 pr-1 font-mono text-[11px] text-subtle ${actions.readOnly || boxes ? "" : "group-hover/s:hidden group-aria-selected/s:hidden [@media(hover:none)]:hidden"}`}>
        {shelf === "snoozed" ? snoozeLeft(row.at, now) : shortAgo(row.at, now)}
      </span>
      {!actions.readOnly && !boxes && (
        <button
          type="button"
          tabIndex={-1}
          title={`${r.label} (⌘↵)`}
          aria-label={`${r.label} ${t.title}`}
          onClick={(e) => {
            e.stopPropagation();
            void actions.restore(t);
          }}
          className="hidden h-6 shrink-0 items-center gap-1 rounded-md px-1.5 text-[12px] text-subtle transition group-hover/s:flex group-aria-selected/s:flex hover:bg-fg/[0.07] hover:text-fg [@media(hover:none)]:flex"
        >
          <Restore className="size-3.5" strokeWidth={1.5} />
          {r.label}
        </button>
      )}
    </div>
  );
}
