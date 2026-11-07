"use client";

import { Funnel, ArrowDownFromLine, ArrowDownWideNarrow, BellRing, ArrowRightFromLine, ArrowUpNarrowWide, Bookmark, ChevronDown, ChevronRight, Columns3, Focus as FocusIcon, Network, Pencil, Pin as PinIcon, PinOff, Plus, Rows3, Search, SlidersHorizontal, Trash2, X, ArrowLeft, ArrowRight, FileDiff as DiffIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { AgentsServices } from "@/lib/agents-services";
import type { AgentsData } from "@/lib/agents-store";
import {
  addView,
  ALL,
  directionOf,
  HIDEABLE,
  isDefaultDisplay,
  defaultDisplay,
  withDirection,
  type Hideable,
  countView,
  deleteView,
  encodeDoc,
  isEmptyFilter,
  isPinned,
  loadCurrentView,
  loadDoc,
  MAX_NAME,
  MAX_TEXT,
  MAX_VIEWS,
  moveView,
  newViewId,
  parseDoc,
  pin,
  renameView,
  resizePin,
  saveCurrentView,
  saveDoc,
  setViewDisplay,
  setViewFilter,
  setViewOrder,
  STATUS_FILTERS,
  statusFilterLabel,
  unpin,
  viewById,
  viewsOf,
  type CanvasDoc,
  type Display,
  type Pin,
  type PinKind,
  type ViewFilter,
  type ViewLike,
  GROUP_BYS,
  groupByOf,
  layoutOf,
  LAYOUTS,
  SORT_KEYS,
  sortOf,
  subGroupByOf,
  withLayout,
  type Layout,
} from "@/lib/agents-views";
import { firstDir, groupLabel, sortLabel } from "@/lib/agents-collection";
import type { Order } from "@/lib/agents-layout";
import { Menu } from "./menu";
import { ShelfControl } from "./project-shelf";
import { openShelf } from "@/lib/agents-shelf";
import { openPalette } from "@/lib/agents-palette";
import { Popover, PopoverPopup, PopoverTrigger, Segmented, Select, Switch } from "./ui";
import { SlidingThumb } from "./ui/thumb";
import { useReducedMotion } from "@roll/brand";
import { AnimatePresence, motion, type Transition } from "motion/react";
import { POPOVER_IN, POPOVER_OUT } from "@/lib/agents-motion";
import { useEnteringValue, usePresenceTracks } from "./presence";
import { useToolsArmed } from "./card-tools";

/** How long edits wait before they go to roll (typing a filter, resizing a card). */
const SAVE_AFTER_MS = 350;

export interface CanvasViews {
  doc: CanvasDoc;
  /** All, then the saved views, in the bar's order. */
  views: ViewLike[];
  /** The view this device shows. */
  current: ViewLike;
  /** What narrows it now: the view's filter, or on All this device's unsaved one. */
  filter: ViewFilter;
  select: (id: string) => void;
  setFilter: (f: ViewFilter) => void;
  /** Save `filter` as a new view and show it. */
  saveAs: (name: string, filter: ViewFilter) => void;
  update: (fn: (d: CanvasDoc) => CanvasDoc) => void;
  setOrder: (order: Order) => void;
  /** The shown view's display options (direction…), saved with it. */
  setDisplay: (display: Display) => void;
  togglePin: (kind: PinKind, id: string, exists?: (p: Pin) => boolean) => void;
  resize: (p: Pin, w: number, h: number) => void;
}

/**
 * The canvas document (`{ order, pins, views }`, `lib/agents-views.ts`), kept here and by roll so
 * every device shows the same views. Edits are the user's intent now, so the last writer wins;
 * another device's save arrives as a newer `revision` on the stream and loads, unless an edit of
 * ours is about to go out. Which view shows is per device.
 */
export function useCanvasViews(canvas: AgentsServices["canvas"] | undefined, owner: string, revision: number): CanvasViews {
  const [doc, setDoc] = useState<CanvasDoc>(() => loadDoc(owner));
  const docRef = useRef(doc);
  const [currentId, setCurrentId] = useState(() => loadCurrentView(owner));
  const [draft, setDraft] = useState<ViewFilter>({});
  const known = useRef(0);
  const pending = useRef<{ timer: ReturnType<typeof setTimeout>; doc: CanvasDoc } | null>(null);
  const canvasRef = useRef(canvas);
  canvasRef.current = canvas;

  useEffect(() => {
    if (!canvas || (revision > 0 && revision <= known.current)) return;
    let live = true;
    canvas
      .load(owner)
      .then((saved) => {
        if (!live || !saved || saved.revision <= known.current) return;
        known.current = saved.revision;
        if (pending.current) return;
        const d = parseDoc(saved.doc);
        docRef.current = d;
        setDoc(d);
        saveDoc(owner, d);
      })
      .catch(() => {
        // Kept on this device until roll answers.
      });
    return () => {
      live = false;
    };
  }, [canvas, owner, revision]);

  const flush = useCallback(() => {
    const p = pending.current;
    if (!p) return;
    clearTimeout(p.timer);
    pending.current = null;
    canvasRef.current
      ?.save(owner, encodeDoc(p.doc))
      .then((r) => (known.current = Math.max(known.current, r.revision)))
      .catch(() => {
        // Kept on this device; the next edit tries again.
      });
  }, [owner]);
  useEffect(() => {
    window.addEventListener("pagehide", flush);
    return () => {
      window.removeEventListener("pagehide", flush);
      flush();
    };
  }, [flush]);

  const update = useCallback(
    (fn: (d: CanvasDoc) => CanvasDoc) => {
      const next = fn(docRef.current);
      if (next === docRef.current) return;
      docRef.current = next;
      setDoc(next);
      saveDoc(owner, next);
      if (pending.current) clearTimeout(pending.current.timer);
      pending.current = { doc: next, timer: setTimeout(flush, SAVE_AFTER_MS) };
    },
    [owner, flush],
  );

  // Kept while the document is (All is made afresh each time): the canvas renders again only for a change.
  const current = useMemo(() => viewById(doc, currentId), [doc, currentId]);
  const select = useCallback(
    (id: string) => {
      setCurrentId(id);
      saveCurrentView(owner, id);
    },
    [owner],
  );
  const id = current.id;
  const filter = id === ALL ? draft : current.filter;
  const setFilter = useCallback((f: ViewFilter) => (id === ALL ? setDraft(f) : update((d) => setViewFilter(d, id, f))), [id, update]);
  const saveAs = useCallback(
    (name: string, f: ViewFilter) => {
      const vid = newViewId();
      let made = false;
      update((d) => {
        // What's on screen keeps looking the same as a view.
        const next = addView(d, name, f, vid, viewById(d, id).display);
        made = next !== d;
        return next;
      });
      if (!made) return;
      if (id === ALL) setDraft({});
      select(vid);
    },
    [update, select, id],
  );
  const setOrder = useCallback((o: Order) => update((d) => setViewOrder(d, id, o)), [id, update]);
  const setDisplay = useCallback((display: Display) => update((d) => setViewDisplay(d, id, display)), [id, update]);
  const togglePin = useCallback(
    (kind: PinKind, target: string, exists?: (p: Pin) => boolean) =>
      update((d) => (isPinned(viewById(d, id), kind, target) ? unpin(d, id, kind, target) : pin(d, id, kind, target, exists))),
    [id, update],
  );
  const resize = useCallback((p: Pin, w: number, h: number) => update((d) => resizePin(d, id, p.kind, p.id, w, h)), [id, update]);
  const views = useMemo(() => viewsOf(doc), [doc]);
  return useMemo(
    () => ({ doc, views, current, filter, select, setFilter, saveAs, update, setOrder, setDisplay, togglePin, resize }),
    [doc, views, current, filter, select, setFilter, saveAs, update, setOrder, setDisplay, togglePin, resize],
  );
}

const typing = (el: Element | null) => !!el && ((el as HTMLElement).isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || !!el.closest(".xterm"));

type ViewRequest = "new" | "rename";
const viewRequests = new Set<(r: ViewRequest) => void>();

/** ⌘K's "New view…" and "Rename view…": the bar opens its editor or the tab's name field. */
export function requestView(r: ViewRequest) {
  for (const fn of viewRequests) fn(r);
}

/**
 * The first card a filter shows, in reading order (top to bottom, then left to right): ↓ or Enter
 * in the filter field goes there.
 */
export function firstShownCard(root: ParentNode = document): HTMLElement | null {
  const cards = [...root.querySelectorAll<HTMLElement>('[data-card][data-key^="t:"]:not([data-leaving])')];
  const at = (el: HTMLElement) => el.getBoundingClientRect();
  cards.sort((a, b) => at(a).top - at(b).top || at(a).left - at(b).left);
  return cards[0] ?? null;
}

/**
 * The views bar over the canvas: All and the owner's views as tabs (with how many threads each
 * picks), a filter field and the filter editor, and saving what's filtered as a view. Keys (not
 * while typing): ⌥1…⌥9 show a view, [ and ] the previous and next, / filters, ⌥S saves the
 * filter as a view.
 */
export function ViewsBar({
  views: v,
  data,
  active,
  phone,
  focused,
  reveal,
}: {
  views: CanvasViews;
  data: AgentsData;
  active: boolean;
  phone: boolean;
  /** The project filling the canvas: a chip that lets go of it. */
  focused?: { id: string; name: string; clear: () => void };
  /** Bring a card into view (↓ / Enter from the filter field goes to the first result). */
  reveal?: (key: string) => void;
}) {
  const [editor, setEditor] = useState<null | { mode: "new" | "edit" }>(null);
  // Each opening of the editor is a fresh one (reopened while the last fades, the two cross).
  const opened = useRef({ was: false, n: 0 });
  if (editor && !opened.current.was) opened.current.n++;
  opened.current.was = !!editor;
  const [renaming, setRenaming] = useState<string | null>(null);
  const [dragging, setDragging] = useState<string | null>(null);
  const field = useRef<HTMLInputElement>(null);
  const tabs = useRef<HTMLDivElement>(null);
  const bar = useRef<HTMLDivElement>(null);
  const now = Date.now();
  const onAll = v.current.id === ALL;
  const draftActive = onAll && !isEmptyFilter(v.filter);

  const { views, select } = v;
  const currentId = v.current.id;
  const { setDisplay } = v;
  const shownDisplay = v.current.display;
  const setLayout = useCallback(
    (l: Layout) => {
      setDisplay(withLayout(shownDisplay, l));
      // The list or board takes the keys at once (j/k, x, Enter): focused as soon as it's in (it
      // renders deferred), unless something else took focus meanwhile.
      if (l !== "canvas") {
        let tries = 30;
        const take = () => {
          const a = document.activeElement;
          if (a && a !== document.body && !a.closest('[aria-label="Show as"]')) return;
          const el = document.querySelector<HTMLElement>("[data-thread-list], [data-thread-board]");
          if (el) el.focus({ preventScroll: true });
          else if (tries-- > 0) requestAnimationFrame(take);
        };
        requestAnimationFrame(take);
      }
    },
    [setDisplay, shownDisplay],
  );
  useEffect(() => {
    const on = (r: ViewRequest) => {
      if (r === "new") setEditor({ mode: "new" });
      else if (currentId !== ALL) setRenaming(currentId);
    };
    viewRequests.add(on);
    return () => void viewRequests.delete(on);
  }, [currentId]);
  // ↓ / Enter in the filter field: the first card it shows.
  const toResults = () => {
    const card = firstShownCard(bar.current?.parentElement ?? document);
    if (!card) return false;
    (card.querySelector<HTMLElement>("[data-drag][tabindex='0']") ?? card).focus({ preventScroll: true });
    if (card.dataset.key) reveal?.(card.dataset.key);
    return true;
  };
  useEffect(() => {
    if (!active) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.defaultPrevented || e.metaKey || e.ctrlKey || typing(e.target as Element) || (e.target as Element | null)?.closest?.("[role=dialog]:not([data-surface]), [role=menu]")) return;
      const i = views.findIndex((x) => x.id === v.current.id);
      if (e.altKey && /^Digit[1-9]$/.test(e.code)) {
        const to = views[Number(e.code.slice(5)) - 1];
        if (to) select(to.id);
      } else if (e.altKey && e.code === "KeyS") setEditor({ mode: "new" });
      else if (e.altKey) return;
      else if (e.key === "[" && i > 0) select(views[i - 1].id);
      else if (e.key === "]" && i < views.length - 1) select(views[i + 1].id);
      else if (e.key === "/") field.current?.focus();
      // The shelf: settled, snoozed and archived conversations (the focused project's, if one fills the canvas).
      else if (e.shiftKey && e.code === "KeyS") openShelf(focused?.id ?? null);
      // ⇧C ⇧L ⇧B: the view as the canvas, a list or a board.
      else if (e.shiftKey && LAYOUT_KEYS[e.code]) setLayout(LAYOUT_KEYS[e.code]);
      // F: "Focus a project…", as the palette's row says.
      else if (!e.shiftKey && e.code === "KeyF") openPalette({ kind: "focus-project" });
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, views, select, v.current.id, focused?.id, setLayout]);

  // The shown tab stays in view in a bar that scrolls.
  useEffect(() => {
    tabs.current?.querySelector<HTMLElement>("[aria-selected=true]")?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [v.current.id]);

  const saved = v.doc.views;
  const move = (id: string, by: number) => v.update((d) => moveView(d, id, d.views.findIndex((x) => x.id === id) + by));
  const filters = filterCount(v.filter);

  return (
    <div ref={bar} className="@container/views chrome-in relative z-10 flex h-11 shrink-0 items-center gap-2 border-b border-line px-2" data-nodrag>
      <div ref={tabs} role="tablist" aria-label="Canvas views" className="relative flex min-w-36 flex-[1_1_auto] items-center @max-[520px]/views:min-w-0 gap-0.5 overflow-x-auto [scrollbar-width:none]">
        {/* The selected tab's raised look slides from tab to tab (it scrolls with them). */}
        <SlidingThumb selector="[data-tab][data-selected]" className="rounded-md border border-line-2 bg-panel-2 shadow-card inset-shadow-highlight" />
        {views.map((view, i) => {
          const selected = view.id === v.current.id;
          const count = view.id === ALL || !threadFilter(view.filter) ? null : countView(data, view.filter, now);
          const waiting = !!view.filter.status?.includes("waiting");
          if (renaming === view.id)
            return (
              <NameField
                key={view.id}
                initial={view.name}
                label="View name"
                onDone={(name) => {
                  setRenaming(null);
                  if (name !== null) v.update((d) => renameView(d, view.id, name));
                }}
              />
            );
          return (
            <div
              key={view.id}
              data-tab
              data-selected={selected || undefined}
              className={`group/tab relative flex shrink-0 items-center rounded-md border border-transparent transition-colors duration-150 ${selected ? "text-fg" : `text-muted hover:bg-fg/[0.04] hover:text-fg ${waiting && count ? "" : "@max-[520px]/views:hidden"}`} ${dragging === view.id ? "opacity-50" : ""}`}
              draggable={view.id !== ALL}
              onDragStart={(e) => {
                setDragging(view.id);
                e.dataTransfer.effectAllowed = "move";
                e.dataTransfer.setData("text/x-roll-view", view.id);
              }}
              onDragEnd={() => setDragging(null)}
              onDragOver={(e) => {
                if (dragging && dragging !== view.id) e.preventDefault();
              }}
              onDrop={(e) => {
                e.preventDefault();
                const from = e.dataTransfer.getData("text/x-roll-view") || dragging;
                setDragging(null);
                if (from && from !== view.id) v.update((d) => moveView(d, from, view.id === ALL ? 0 : d.views.findIndex((x) => x.id === view.id)));
              }}
            >
              <button
                role="tab"
                aria-selected={selected}
                onClick={() => select(view.id)}
                onDoubleClick={() => view.id !== ALL && setRenaming(view.id)}
                title={i < 9 ? `${view.name} (⌥${i + 1})` : view.name}
                className="flex h-[26px] items-center gap-1.5 px-2.5 text-[12.5px] font-[510] tracking-[-0.01em] whitespace-nowrap"
              >
                {/* A view saved as a list or a board says so. */}
                {view.display.layout === "list" ? <Rows3 className="size-3 text-subtle" aria-hidden /> : view.display.layout === "board" ? <Columns3 className="size-3 text-subtle" aria-hidden /> : null}
                {/* Kept beside the shown tab on a narrow strip, a waiting view is its bell and count (its name is the title). */}
                {!selected && waiting && <BellRing aria-hidden className="hidden size-3.5 text-accent @max-[520px]/views:block" />}
                <span className={!selected && waiting ? "@max-[520px]/views:sr-only" : undefined}>{view.name}</span>
                {count !== null && count > 0 && (
                  <span className={`grid h-4 min-w-4 place-items-center rounded-full px-1 font-mono text-[10px] tabular-nums ${waiting ? "bg-accent/15 text-accent" : "bg-fg/[0.07] text-muted"}`} aria-label={`${count} thread${count === 1 ? "" : "s"}`}>
                    {count}
                  </span>
                )}
              </button>
              {selected && view.id !== ALL && (
                <Menu
                  label={`${view.name} options`}
                  align="start"
                  className="-ml-1.5 mr-0.5 [&>button]:size-5"
                  trigger={<ChevronDown className="size-3" />}
                  items={[
                    { label: "Rename", icon: <Pencil />, onSelect: () => setRenaming(view.id) },
                    { label: "Edit filters", icon: <SlidersHorizontal />, onSelect: () => setEditor({ mode: "edit" }) },
                    saved[0]?.id !== view.id && { label: "Move left", icon: <ArrowLeft />, onSelect: () => move(view.id, -1) },
                    saved.at(-1)?.id !== view.id && { label: "Move right", icon: <ArrowRight />, onSelect: () => move(view.id, 1) },
                    {
                      label: "Delete view",
                      icon: <Trash2 />,
                      danger: true,
                      confirm: `Delete “${view.name}”?`,
                      onSelect: () => {
                        select(ALL);
                        v.update((d) => deleteView(d, view.id));
                      },
                    },
                  ]}
                />
              )}
            </div>
          );
        })}
        {/* Short of room (a phone, a conversation open beside a narrow canvas), only the shown view's
            tab stays, with a waiting view that has threads (what a check-in is for); the others are
            one menu away: tabs cut mid-word read as broken. */}
        <span className="hidden shrink-0 @max-[520px]/views:flex">
          <Menu
            label="Other views"
            align="start"
            trigger={<ChevronDown className="size-3.5" />}
            items={views
              .filter((x) => x.id !== currentId)
              .map((x) => {
                const n = x.id === ALL || !threadFilter(x.filter) ? 0 : countView(data, x.filter, now);
                return { label: n ? `${x.name} · ${n}` : x.name, onSelect: () => select(x.id) };
              })}
          />
        </span>
        {/* Stuck at the strip's end, so it stays in reach however many views scroll under it. */}
        {saved.length < MAX_VIEWS && (
          <button onClick={() => setEditor({ mode: "new" })} title="New view (⌥S)" aria-label="New view" className="sticky right-0 grid size-7 shrink-0 place-items-center rounded-md bg-canvas text-subtle transition-colors duration-150 hover:bg-panel-2 hover:text-fg">
            <Plus className="size-3.5" strokeWidth={1.75} />
          </button>
        )}
      </div>
      {/* Short of room (a conversation open beside it), the filter gives way before the tabs and their +. */}
      <div className="flex min-w-0 shrink-[12] items-center gap-1">
        {focused && (
          // Inside a project's space: the way back out, as a breadcrumb.
          // The project's name never gives way: "All projects" does first.
          <nav aria-label="Where the canvas is" className="fade-in flex h-7 max-w-80 shrink-0 items-center gap-1 rounded-md border border-line-2 bg-panel-2 pr-0.5 pl-1 text-[12px] text-fg inset-shadow-highlight">
            <button onClick={focused.clear} title="Every project (Esc)" className="flex h-6 min-w-0 items-center gap-1.5 rounded px-1 text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg">
              <FocusIcon className="size-3.5 shrink-0" strokeWidth={1.75} aria-hidden />
              <span className="truncate">All projects</span>
            </button>
            <ChevronRight className="size-3 shrink-0 text-subtle" aria-hidden />
            <span className="max-w-48 shrink-0 truncate" aria-current="page">
              {focused.name}
            </span>
            <button onClick={focused.clear} title="Show every project (Esc)" aria-label={`Stop focusing ${focused.name}`} className="grid size-6 shrink-0 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg">
              <X className="size-3" />
            </button>
          </nav>
        )}
        {/* Left of the field, so the field stays where it is as they appear. */}
        {draftActive && (
          <>
            <button onClick={() => setEditor({ mode: "new" })} title="Save as a view (⌥S)" className="flex h-7 shrink-0 items-center gap-1 rounded-md bg-fg px-2 text-[12px] font-medium whitespace-nowrap text-bg transition hover:opacity-85">
              <Bookmark className="size-3.5" />
              <span className="max-sm:sr-only">Save view</span>
            </button>
            <button onClick={() => v.setFilter({})} title="Clear the filter" aria-label="Clear the filter" className="grid size-7 shrink-0 place-items-center rounded-md text-subtle hover:bg-panel-2 hover:text-fg">
              <X className="size-3.5" />
            </button>
          </>
        )}
        {!phone && (
          <label className="flex h-7 w-56 min-w-28 shrink items-center @max-[520px]/views:w-7 @max-[520px]/views:min-w-7 @max-[520px]/views:focus-within:w-40 @max-[520px]/views:focus-within:min-w-40 gap-1.5 rounded-md border border-line bg-fg/[0.03] px-2 text-subtle inset-shadow-highlight transition-colors duration-150 hover:border-line-2 focus-within:border-accent/50 focus-within:text-fg">
            <Search className="size-3.5 shrink-0" strokeWidth={1.75} />
            <input
              ref={field}
              value={v.filter.text ?? ""}
              onChange={(e) => v.setFilter({ ...v.filter, text: e.target.value.slice(0, MAX_TEXT) })}
              onKeyDown={(e) => {
                if (e.key === "Escape") {
                  e.stopPropagation();
                  if (v.filter.text) v.setFilter({ ...v.filter, text: "" });
                  else e.currentTarget.blur();
                } else if (e.altKey && e.code === "KeyS") {
                  // ⌥S saves the filter as a view here too (on a Mac it would type ß).
                  e.preventDefault();
                  setEditor({ mode: "new" });
                } else if ((e.key === "ArrowDown" || e.key === "Enter") && !e.altKey && !e.metaKey && !e.ctrlKey && !e.shiftKey) {
                  if (toResults()) e.preventDefault();
                }
              }}
              placeholder={onAll ? "Filter threads…  /" : `Filter ${v.current.name}…`}
              aria-label="Filter threads"
              className="min-w-0 flex-1 bg-transparent text-[12.5px] text-fg outline-none placeholder:text-subtle"
            />
          </label>
        )}
        <LayoutSwitch layout={layoutOf(shownDisplay)} onChange={setLayout} />
        <ShelfControl data={data} focused={focused?.id ?? null} phone={phone} />
        <DisplayMenu display={v.current.display} phone={phone} onChange={v.setDisplay} />
        <button
          onClick={() => setEditor((e) => (e ? null : { mode: "edit" }))}
          aria-expanded={!!editor}
          aria-label={onAll ? "Filters" : `${v.current.name} filters`}
          title="Filters"
          className={`flex h-7 items-center gap-1 rounded-md px-2 text-[12px] transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg ${filters ? "text-fg" : "text-subtle"} ${editor ? "bg-fg/[0.07]" : ""}`}
        >
          <Funnel className="size-3.5" strokeWidth={1.75} />
          <span className="max-sm:sr-only @max-[520px]/views:sr-only">Filter</span>
          {filters > 0 && <span className="font-mono text-[10.5px] text-accent tabular-nums">{filters}</span>}
        </button>
      </div>
      {/* The editor pops in and out (it was gone in a frame) and stays put across tabs: only what's
          inside starts over for another view (it replayed its pop on every tab). */}
      <AnimatePresence>
        {editor && (
          <FilterShell key={`${editor.mode}:${opened.current.n}`} bar={bar} label={editor.mode === "new" ? "New view" : "View filters"} onClose={() => setEditor(null)}>
            <FilterEditor
              key={v.current.id}
              data={data}
              mode={editor.mode === "new" ? "new" : onAll ? "draft" : "edit"}
              name={editor.mode === "new" ? "" : v.current.name}
              filter={v.filter}
              onChange={editor.mode === "new" ? undefined : v.setFilter}
              onClose={() => setEditor(null)}
              onSave={(name, f) => {
                setEditor(null);
                v.saveAs(name, f);
              }}
            />
          </FilterShell>
        )}
      </AnimatePresence>
    </div>
  );
}

const threadFilter = (f: ViewFilter) => !!(f.status?.length || f.providers?.length || f.text?.trim());
const filterCount = (f: ViewFilter) => (f.machines?.length ?? 0) + (f.projects?.length ?? 0) + (f.status?.length ?? 0) + (f.providers?.length ?? 0) + (f.text?.trim() ? 1 : 0);

const HIDE_LABEL: Record<Hideable, { label: string; hint: string }> = {
  idle: { label: "Idle threads", hint: "Not working, not waiting, nothing new" },
  terminals: { label: "Terminals", hint: "Shells and logins on the canvas" },
};

/**
 * Linear's display options for the view showing (saved with it, so every device agrees): which way
 * it grows (this screen's; a phone keeps its own), density, grouping, and what's shown.
 */
function DisplayMenu({ display, phone, onChange }: { display: Display; phone: boolean; onChange: (d: Display) => void }) {
  const direction = directionOf(display, phone);
  const set = (patch: Partial<Display>) => onChange({ ...display, ...patch });
  const hidden = new Set(display.hide ?? []);
  const changed = !isDefaultDisplay(display);
  return (
    <Popover>
      <PopoverTrigger
        render={
          <button
            type="button"
            aria-label="Display"
            title="Display"
            className={`flex h-7 items-center gap-1 rounded-md px-2 text-[12px] transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg data-popup-open:bg-fg/[0.07] ${changed ? "text-fg" : "text-subtle"}`}
          />
        }
      >
        <SlidersHorizontal className="size-3.5" strokeWidth={1.75} />
        <span className="max-sm:sr-only @max-[520px]/views:sr-only">Display</span>
      </PopoverTrigger>
      <PopoverPopup side="bottom" align="end" sideOffset={6} aria-label="Display options" className="w-[300px] flex-col gap-3 p-3 text-[12.5px]">
        {layoutOf(display) !== "canvas" ? (
          <CollectionDisplay display={display} set={set} />
        ) : (
        <>
        <DisplayRow label="Layout" hint={phone ? "On phones" : undefined}>
          <Segmented
            label="Layout"
            value={direction}
            onChange={(d) => onChange(withDirection(display, phone, d))}
            options={[
              { value: "horizontal", label: <><ArrowRightFromLine /> Across</>, title: "Left to right (⇧D)" },
              { value: "vertical", label: <><ArrowDownFromLine /> Down</>, title: "Top-down, a column per project (⇧D)" },
            ]}
          />
        </DisplayRow>
        <DisplayRow label="Density">
          <Segmented
            label="Density"
            value={display.density ?? "comfortable"}
            onChange={(d) => set({ density: d === "compact" ? "compact" : undefined })}
            options={[
              { value: "comfortable", label: "Comfortable" },
              { value: "compact", label: "Compact" },
            ]}
          />
        </DisplayRow>
        <DisplayRow label="Grouping">
          <Segmented
            label="Grouping"
            value={display.group ?? "machine"}
            onChange={(g) => set({ group: g === "project" ? "project" : undefined })}
            options={[
              { value: "machine", label: "Machine", title: "Machines, then their projects" },
              { value: "project", label: "Project", title: "Projects as the anchors" },
            ]}
          />
        </DisplayRow>
        <div className="border-t border-line pt-2.5" role="group" aria-label="Show">
          <p className="mb-1.5 text-[11px] font-medium tracking-wide text-subtle uppercase">Show</p>
          {HIDEABLE.map((h) => (
            <label key={h} className="flex h-8 cursor-pointer items-center gap-2" title={HIDE_LABEL[h].hint}>
              <span className="min-w-0 flex-1 truncate text-fg">{HIDE_LABEL[h].label}</span>
              <Switch
                size="sm"
                aria-label={HIDE_LABEL[h].label}
                checked={!hidden.has(h)}
                onCheckedChange={(on) => {
                  const next = HIDEABLE.filter((x) => (x === h ? !on : hidden.has(x)));
                  set({ hide: next.length ? next : undefined });
                }}
              />
            </label>
          ))}
          <label className="flex h-8 cursor-pointer items-center gap-2" title="Projects with no thread on the canvas get a card too (otherwise their machine lists them)">
            <span className="min-w-0 flex-1 truncate text-fg">Projects without threads</span>
            <Switch size="sm" aria-label="Projects without threads" checked={display.projects === "all"} onCheckedChange={(on) => set({ projects: on ? "all" : undefined })} />
          </label>
        </div>
        </>
        )}
        {changed && (
          <button type="button" onClick={() => onChange(defaultDisplay(display))} className="self-start text-[12px] text-subtle transition-colors hover:text-fg">
            Reset to default
          </button>
        )}
      </PopoverPopup>
    </Popover>
  );
}

const LAYOUT_KEYS: Record<string, Layout> = { KeyC: "canvas", KeyL: "list", KeyB: "board" };
const LAYOUT_LABEL: Record<Layout, { label: string; key: string; icon: ReactNode }> = {
  canvas: { label: "Canvas", key: "⇧C", icon: <Network /> },
  list: { label: "List", key: "⇧L", icon: <Rows3 /> },
  board: { label: "Board", key: "⇧B", icon: <Columns3 /> },
};

/** Canvas · List · Board for the view showing (saved with it): Linear's view switcher. */
function LayoutSwitch({ layout, onChange }: { layout: Layout; onChange: (l: Layout) => void }) {
  return (
    <span role="radiogroup" aria-label="Show as" className="relative flex h-7 items-center rounded-md border border-line p-px">
      <SlidingThumb selector="[data-layout][aria-checked=true]" className="rounded-[5px] bg-fg/[0.08]" />
      {LAYOUTS.map((l) => (
        <button
          key={l}
          type="button"
          role="radio"
          data-layout={l}
          aria-checked={layout === l}
          aria-label={LAYOUT_LABEL[l].label}
          title={`${LAYOUT_LABEL[l].label} (${LAYOUT_LABEL[l].key})`}
          onClick={() => onChange(l)}
          className={`relative grid h-full w-7 place-items-center rounded-[5px] transition-colors duration-150 [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${layout === l ? "text-fg" : "text-subtle hover:text-fg"}`}
        >
          {LAYOUT_LABEL[l].icon}
        </button>
      ))}
    </span>
  );
}

/** The list's and the board's display: grouping, sub-grouping, and the order inside a group. */
function CollectionDisplay({ display, set }: { display: Display; set: (patch: Partial<Display>) => void }) {
  const by = groupByOf(display);
  const sub = subGroupByOf(display);
  const sort = sortOf(display);
  const board = display.layout === "board";
  return (
    <>
      <DisplayRow label={board ? "Columns" : "Grouping"}>
        <Select label={board ? "Columns" : "Grouping"} variant="ghost" className="min-w-32 justify-between" value={by} onChange={(g) => set({ groupBy: g === "status" ? undefined : g, ...(g === sub ? { subGroupBy: undefined } : {}) })} options={GROUP_BYS.map((g) => ({ value: g, label: groupLabel[g] }))} />
      </DisplayRow>
      <DisplayRow label={board ? "Swimlanes" : "Sub-grouping"}>
        <Select label={board ? "Swimlanes" : "Sub-grouping"} variant="ghost" className="min-w-32 justify-between" value={sub} onChange={(g) => set({ subGroupBy: g === "none" ? undefined : g })} options={GROUP_BYS.filter((g) => g !== by || g === "none").map((g) => ({ value: g, label: groupLabel[g] }))} />
      </DisplayRow>
      <DisplayRow label="Ordering">
        <span className="flex items-center gap-1">
          <Select label="Ordering" variant="ghost" className="min-w-28 justify-between" value={sort.key} onChange={(k) => set({ sort: k === "activity" && sort.dir === "desc" ? undefined : { key: k, dir: sort.key === k ? sort.dir : firstDir(k) } })} options={SORT_KEYS.map((k) => ({ value: k, label: sortLabel[k] }))} />
          <button
            type="button"
            onClick={() => {
              const dir = sort.dir === "asc" ? "desc" : "asc";
              set({ sort: sort.key === "activity" && dir === "desc" ? undefined : { key: sort.key, dir } });
            }}
            aria-label={sort.dir === "asc" ? "Ascending" : "Descending"}
            title={sort.dir === "asc" ? "Ascending" : "Descending"}
            className="grid size-7 place-items-center rounded-md text-muted hover:bg-fg/[0.06] hover:text-fg"
          >
            {sort.dir === "asc" ? <ArrowUpNarrowWide className="size-3.5" /> : <ArrowDownWideNarrow className="size-3.5" />}
          </button>
        </span>
      </DisplayRow>
    </>
  );
}

function DisplayRow({ label, hint, children }: { label: string; hint?: string; children: ReactNode }) {
  return (
    <div className="flex items-center gap-2">
      <span className="min-w-0 flex-1 text-muted">
        {label}
        {hint && <span className="ml-1.5 text-[11px] text-subtle">{hint}</span>}
      </span>
      {children}
    </div>
  );
}

function NameField({ initial, label, onDone }: { initial: string; label: string; onDone: (name: string | null) => void }) {
  const [name, setName] = useState(initial);
  return (
    <input
      autoFocus
      value={name}
      maxLength={MAX_NAME}
      aria-label={label}
      onChange={(e) => setName(e.target.value)}
      onFocus={(e) => e.currentTarget.select()}
      onBlur={() => onDone(name.trim() ? name : null)}
      onKeyDown={(e) => {
        if (e.key === "Enter") onDone(name.trim() ? name : null);
        else if (e.key === "Escape") {
          e.stopPropagation();
          onDone(null);
        }
      }}
      className="h-7 w-36 shrink-0 rounded-md border border-accent/50 bg-bg px-2 text-[12.5px] outline-none"
    />
  );
}

/**
 * What a view shows, as toggles: status, machines, projects (of the machines picked, if any),
 * agents and a text match. Editing a view changes it as you go; on All it's this device's filter
 * until it's saved as a view; a new view starts from what All is filtered by.
 */
function FilterEditor({
  data,
  mode,
  name: initialName,
  filter: initial,
  onChange,
  onSave,
  onClose,
}: {
  data: AgentsData;
  mode: "new" | "draft" | "edit";
  name: string;
  filter: ViewFilter;
  onChange?: (f: ViewFilter) => void;
  onSave: (name: string, f: ViewFilter) => void;
  onClose: () => void;
}) {
  const [own, setOwn] = useState<ViewFilter>(initial);
  const [name, setName] = useState(mode === "new" ? "" : initialName);
  const f = onChange ? initial : own;
  const set = (next: ViewFilter) => (onChange ? onChange(next) : setOwn(next));

  const toggle = <K extends "machines" | "projects" | "status" | "providers">(k: K, value: string) => {
    const list = (f[k] ?? []) as string[];
    set({ ...f, [k]: list.includes(value) ? list.filter((x) => x !== value) : [...list, value] });
  };
  const machines = [...data.machines.values()].map((r) => r.value).sort((a, b) => a.name.localeCompare(b.name));
  const projects = [...data.projects.values()]
    .filter((r) => !f.machines?.length || f.machines.includes(r.machine))
    .sort((a, b) => a.value.name.localeCompare(b.value.name));
  const providers = new Map<string, string>();
  for (const m of machines) for (const p of m.info?.providers ?? []) if (p.installed) providers.set(p.id, p.name);
  for (const r of data.threads.values()) if (!providers.has(r.value.provider)) providers.set(r.value.provider, r.value.provider);
  const save = () => name.trim() && !isEmptyFilter(f) && onSave(name, f);

  return (
    <>
      {mode === "new" ? (
        <input
          autoFocus
          value={name}
          maxLength={MAX_NAME}
          onChange={(e) => setName(e.target.value)}
          onKeyDown={(e) => e.key === "Enter" && save()}
          placeholder="Name this view"
          aria-label="View name"
          className="mb-3 h-8 w-full rounded-md border border-line bg-bg px-2.5 text-[13px] outline-none focus:border-line-2"
        />
      ) : (
        <p className="mb-3 text-[13px] font-medium tracking-[-0.01em]">{mode === "draft" ? "Filter threads" : `${initialName}: what it shows`}</p>
      )}
      <Section label="Status">
        {STATUS_FILTERS.map((s) => (
          <Chip key={s} on={!!f.status?.includes(s)} onClick={() => toggle("status", s)}>
            {statusFilterLabel[s]}
          </Chip>
        ))}
      </Section>
      {machines.length > 1 && (
        <Section label="Machines">
          {machines.map((m) => (
            <Chip key={m.id} on={!!f.machines?.includes(m.id)} onClick={() => toggle("machines", m.id)}>
              {m.name}
            </Chip>
          ))}
        </Section>
      )}
      {projects.length > 0 && (
        <Section label="Projects">
          {projects.map((r) => (
            <Chip key={r.value.id} on={!!f.projects?.includes(r.value.id)} onClick={() => toggle("projects", r.value.id)}>
              {r.value.name}
              {machines.length > 1 && <span className="ml-1 text-subtle">{data.machines.get(r.machine)?.value.name}</span>}
            </Chip>
          ))}
        </Section>
      )}
      {providers.size > 1 && (
        <Section label="Agents">
          {[...providers].map(([id, label]) => (
            <Chip key={id} on={!!f.providers?.includes(id)} onClick={() => toggle("providers", id)}>
              {label}
            </Chip>
          ))}
        </Section>
      )}
      <Section label="Text">
        <input
          value={f.text ?? ""}
          onChange={(e) => set({ ...f, text: e.target.value.slice(0, MAX_TEXT) })}
          placeholder="Title, project or machine"
          aria-label="Text to match"
          className="h-7 w-full rounded-md border border-line bg-bg px-2 text-[12.5px] outline-none focus:border-line-2"
        />
      </Section>
      <div className="mt-3 flex items-center justify-end gap-1.5 border-t border-line pt-3">
        {!isEmptyFilter(f) && (
          <button onClick={() => set({})} className="mr-auto text-[12px] text-subtle hover:text-fg">
            Clear
          </button>
        )}
        {mode === "edit" ? (
          <button onClick={onClose} className="h-7 rounded-md bg-fg px-3 text-[12px] font-medium text-bg hover:opacity-85">
            Done
          </button>
        ) : mode === "draft" ? (
          // One footer: what's filtered here can be kept as a view; a press outside or Escape closes it.
          <SaveDraft onSave={(n) => onSave(n, f)} disabled={isEmptyFilter(f)} />
        ) : (
          <>
            <button onClick={onClose} className="h-7 rounded-md px-3 text-[12px] text-muted hover:bg-panel-2 hover:text-fg">
              Cancel
            </button>
            <button
              onClick={save}
              disabled={!name.trim() || isEmptyFilter(f)}
              title={isEmptyFilter(f) ? "Pick what it shows first: a view with no filter is All" : undefined}
              className="h-7 rounded-md bg-fg px-3 text-[12px] font-medium text-bg hover:opacity-85 disabled:opacity-40"
            >
              Create view
            </button>
          </>
        )}
      </div>
    </>
  );
}

/**
 * The filter editor's frame under the views bar: it pops in and out as Base UI's popovers do
 * (`Pop`'s motion), and closes on a press outside it and the bar, or Escape.
 */
function FilterShell({ bar, label, onClose, children }: { bar: React.RefObject<HTMLElement | null>; label: string; onClose: () => void; children: ReactNode }) {
  const box = useRef<HTMLDivElement>(null);
  const reduced = useReducedMotion();
  const none: Transition = { duration: 0 };
  const opacity = useEnteringValue(0, 1);
  const scale = useEnteringValue(0.98, 1);
  const y = useEnteringValue(-2, 0);
  const present = usePresenceTracks(
    [
      { value: opacity, shown: 1, hidden: 0, enter: reduced ? none : POPOVER_IN, leave: reduced ? none : POPOVER_OUT },
      { value: scale, shown: 1, hidden: 0.98, enter: reduced ? none : POPOVER_IN, leave: reduced ? none : POPOVER_OUT },
      { value: y, shown: 0, hidden: -2, enter: reduced ? none : POPOVER_IN, leave: reduced ? none : POPOVER_OUT },
    ],
    String(reduced),
  );
  useEffect(() => {
    if (!present) return;
    const onDown = (e: PointerEvent) => {
      const t = e.target as Element;
      // The bar's own buttons (tabs, filters, new view) act on the editor themselves.
      if (!box.current?.contains(t) && !bar.current?.contains(t)) onClose();
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      e.stopPropagation();
      onClose();
    };
    window.addEventListener("pointerdown", onDown, true);
    window.addEventListener("keydown", onKey, true);
    return () => {
      window.removeEventListener("pointerdown", onDown, true);
      window.removeEventListener("keydown", onKey, true);
    };
  }, [present, onClose, bar]);
  return (
    <motion.div
      ref={box}
      role="dialog"
      aria-label={label}
      data-state={present ? "open" : "closing"}
      inert={!present}
      aria-hidden={!present || undefined}
      className="absolute top-full right-2 z-30 mt-1.5 w-[min(380px,calc(100vw-1rem))] rounded-xl border border-line bg-panel p-3 text-[12.5px] shadow-[0_16px_40px_-16px_rgb(0_0_0/0.55)]"
      style={{ opacity, scale, y, transformOrigin: "top right" }}
    >
      {children}
    </motion.div>
  );
}

/** On All: name what's filtered and keep it as a view. */
function SaveDraft({ onSave, disabled }: { onSave: (name: string) => void; disabled: boolean }) {
  const [name, setName] = useState("");
  return (
    <form
      className="flex min-w-0 flex-1 items-center gap-1.5"
      onSubmit={(e) => {
        e.preventDefault();
        if (name.trim()) onSave(name);
      }}
    >
      <input
        value={name}
        maxLength={MAX_NAME}
        onChange={(e) => setName(e.target.value)}
        placeholder="Name this view"
        aria-label="View name"
        disabled={disabled}
        className="h-7 min-w-0 flex-1 rounded-md border border-line bg-bg px-2 text-[12.5px] outline-none focus:border-line-2 disabled:opacity-40"
      />
      <button type="submit" disabled={disabled || !name.trim()} className="flex h-7 items-center gap-1 rounded-md bg-fg px-2.5 text-[12px] font-medium text-bg hover:opacity-85 disabled:opacity-40">
        <Bookmark className="size-3.5" /> Save as view
      </button>
    </form>
  );
}

function Section({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="mb-2.5" role="group" aria-label={label}>
      <p className="mb-1.5 text-[11px] font-medium tracking-wide text-subtle uppercase">{label}</p>
      <div className="flex flex-wrap gap-1.5">{children}</div>
    </div>
  );
}

function Chip({ on, onClick, children }: { on: boolean; onClick: () => void; children: ReactNode }) {
  return (
    <button
      type="button"
      aria-pressed={on}
      onClick={onClick}
      className={`inline-flex h-7 items-center rounded-full border px-2.5 text-[12px] transition-colors duration-150 ${
        on ? "border-accent/50 bg-accent/12 text-fg" : "border-line text-muted hover:border-line-2 hover:text-fg"
      }`}
    >
      {children}
    </button>
  );
}

/**
 * Pin buttons over a card on the canvas (a thread's conversation and diff, a terminal), shown
 * while the pointer is on it (always on touch screens and keyboard focus).
 */
export function PinTools({
  view,
  items,
  onToggle,
  extra,
  children,
}: {
  view: ViewLike;
  items: { kind: PinKind; id: string; label: string }[];
  onToggle: (kind: PinKind, id: string) => void;
  /** More of the card's tools, after the pins (a terminal's Close). */
  extra?: ReactNode;
  children: ReactNode;
}) {
  // The cards' floating toolbar (`HoverActions` in nodes.tsx), so the two read as one; its
  // buttons mount once they're first needed (`useToolsArmed`).
  const { armed, ref } = useToolsArmed();
  return (
    <div className="group/slot relative">
      {children}
      {/* Above the card, as its own tools are (never over its time or status); beside them when it has some (globals.css); level with them (they sit in its border). */}
      <div
        ref={ref}
        data-pin-tools
        className={`pointer-events-none absolute right-2 bottom-[calc(100%+5px)] z-10 flex translate-y-1 items-center gap-0.5 rounded-lg border border-line-2 p-0.5 text-muted opacity-0 shadow-pop inset-shadow-highlight glass transition-[opacity,translate] duration-150 ease-out starting:translate-y-1! starting:opacity-0! group-hover/slot:pointer-events-auto group-hover/slot:translate-y-0 group-hover/slot:opacity-100 focus-within:pointer-events-auto focus-within:translate-y-0 focus-within:opacity-100 motion-reduce:transition-none ${armed ? "" : "hidden"}`}
        data-nodrag
        data-touch-tools
      >
        {armed &&
          items.map((i) => {
          const pinned = isPinned(view, i.kind, i.id);
          const label = `${pinned ? "Unpin" : "Pin"} ${i.label}${view.id === ALL ? "" : ` on ${view.name}`}`;
          return (
            <button
              key={i.kind}
              type="button"
              data-pin={i.kind}
              aria-label={label}
              title={label}
              aria-pressed={pinned}
              onClick={(e) => {
                e.stopPropagation();
                onToggle(i.kind, i.id);
              }}
              className={`grid size-6 place-items-center rounded-md transition-colors duration-150 hover:bg-panel-3 hover:text-fg [&>svg]:size-3.5 [&>svg]:stroke-[1.75] ${pinned ? "text-accent" : ""}`}
            >
              {i.kind === "diff" ? <DiffIcon /> : pinned ? <PinOff /> : <PinIcon />}
            </button>
          );
        })}
        {armed && extra}
      </div>
    </div>
  );
}
