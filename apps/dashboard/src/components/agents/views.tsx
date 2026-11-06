"use client";

import { ArrowDownFromLine, ArrowRightFromLine, Bookmark, ChevronDown, Focus as FocusIcon, ListFilter, Pencil, Pin as PinIcon, PinOff, Plus, Search, SlidersHorizontal, Trash2, X, ArrowLeft, ArrowRight, FileDiff as DiffIcon } from "lucide-react";
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
} from "@/lib/agents-views";
import type { Order } from "@/lib/agents-layout";
import { Menu } from "./menu";
import { ShelfControl } from "./project-shelf";
import { openShelf } from "@/lib/agents-shelf";
import { Popover, PopoverPopup, PopoverTrigger, Segmented, Switch } from "./ui";

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

  const current = viewById(doc, currentId);
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
  return { doc, views, current, filter, select, setFilter, saveAs, update, setOrder, setDisplay, togglePin, resize };
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
  const cards = [...root.querySelectorAll<HTMLElement>('[data-card][data-key^="t:"]')];
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
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, views, select, v.current.id, focused?.id]);

  // The shown tab stays in view in a bar that scrolls.
  useEffect(() => {
    tabs.current?.querySelector<HTMLElement>("[aria-selected=true]")?.scrollIntoView?.({ block: "nearest", inline: "nearest" });
  }, [v.current.id]);

  const saved = v.doc.views;
  const move = (id: string, by: number) => v.update((d) => moveView(d, id, d.views.findIndex((x) => x.id === id) + by));
  const filters = filterCount(v.filter);

  return (
    <div ref={bar} className="chrome-in relative z-10 flex h-11 shrink-0 items-center gap-2 border-b border-line px-2" data-nodrag>
      <div ref={tabs} role="tablist" aria-label="Canvas views" className="flex min-w-0 flex-1 items-center gap-0.5 overflow-x-auto [scrollbar-width:none]">
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
              className={`group/tab flex shrink-0 items-center rounded-md border transition-colors duration-150 ${selected ? "border-line-2 bg-panel-2 text-fg shadow-card inset-shadow-highlight" : "border-transparent text-muted hover:bg-fg/[0.04] hover:text-fg"} ${dragging === view.id ? "opacity-50" : ""}`}
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
                {view.name}
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
        {saved.length < MAX_VIEWS && (
          <button onClick={() => setEditor({ mode: "new" })} title="New view (⌥S)" aria-label="New view" className="grid size-7 shrink-0 place-items-center rounded-md text-subtle transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg">
            <Plus className="size-3.5" strokeWidth={1.75} />
          </button>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-1">
        {focused && (
          <span className="fade-in flex h-7 max-w-44 items-center gap-1.5 rounded-md border border-line-2 bg-panel-2 pr-0.5 pl-2 text-[12px] text-fg inset-shadow-highlight" role="status">
            <FocusIcon className="size-3.5 shrink-0 text-muted" strokeWidth={1.75} aria-hidden />
            <span className="truncate">
              <span className="text-muted">Only</span> {focused.name}
            </span>
            <button onClick={focused.clear} title="Show every project (Esc)" aria-label={`Stop focusing ${focused.name}`} className="grid size-6 shrink-0 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg">
              <X className="size-3" />
            </button>
          </span>
        )}
        {/* Left of the field, so the field stays where it is as they appear. */}
        {draftActive && (
          <>
            <button onClick={() => setEditor({ mode: "new" })} title="Save as a view (⌥S)" className="flex h-7 items-center gap-1 rounded-md bg-fg px-2 text-[12px] font-medium text-bg transition hover:opacity-85">
              <Bookmark className="size-3.5" />
              <span className="max-sm:sr-only">Save view</span>
            </button>
            <button onClick={() => v.setFilter({})} title="Clear the filter" aria-label="Clear the filter" className="grid size-7 place-items-center rounded-md text-subtle hover:bg-panel-2 hover:text-fg">
              <X className="size-3.5" />
            </button>
          </>
        )}
        {!phone && (
          <label className="flex h-7 w-56 items-center gap-1.5 rounded-md border border-line bg-fg/[0.03] px-2 text-subtle inset-shadow-highlight transition-colors duration-150 hover:border-line-2 focus-within:border-accent/50 focus-within:text-fg">
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
        <ShelfControl data={data} focused={focused?.id ?? null} phone={phone} />
        <DisplayMenu display={v.current.display} phone={phone} onChange={v.setDisplay} />
        <button
          onClick={() => setEditor((e) => (e ? null : { mode: "edit" }))}
          aria-expanded={!!editor}
          aria-label={onAll ? "Filters" : `${v.current.name} filters`}
          title="Filters"
          className={`flex h-7 items-center gap-1 rounded-md px-2 text-[12px] transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg ${filters ? "text-fg" : "text-subtle"} ${editor ? "bg-fg/[0.07]" : ""}`}
        >
          <ListFilter className="size-3.5" strokeWidth={1.75} />
          {filters > 0 && <span className="font-mono text-[10.5px] text-accent tabular-nums">{filters}</span>}
        </button>
      </div>
      {editor && (
        <FilterEditor
          key={`${editor.mode}:${v.current.id}`}
          bar={bar}
          data={data}
          mode={editor.mode === "new" ? "new" : onAll ? "draft" : "edit"}
          name={editor.mode === "new" ? "" : v.current.name}
          filter={v.filter}
          onChange={editor.mode === "new" ? undefined : v.setFilter}
          onSave={(name, f) => {
            setEditor(null);
            v.saveAs(name, f);
          }}
          onClose={() => setEditor(null)}
        />
      )}
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
        <span className="max-sm:sr-only">Display</span>
      </PopoverTrigger>
      <PopoverPopup side="bottom" align="end" sideOffset={6} aria-label="Display options" className="w-[300px] flex-col gap-3 p-3 text-[12.5px]">
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
        {changed && (
          <button type="button" onClick={() => onChange(defaultDisplay(display))} className="self-start text-[12px] text-subtle transition-colors hover:text-fg">
            Reset to default
          </button>
        )}
      </PopoverPopup>
    </Popover>
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
  bar,
}: {
  bar: React.RefObject<HTMLElement | null>;
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
  const box = useRef<HTMLDivElement>(null);
  useEffect(() => {
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
  }, [onClose, bar]);

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
    <div
      ref={box}
      role="dialog"
      aria-label={mode === "new" ? "New view" : "View filters"}
      className="pop absolute top-full right-2 z-30 mt-1.5 w-[min(380px,calc(100vw-1rem))] rounded-xl border border-line bg-panel p-3 text-[12.5px] shadow-[0_16px_40px_-16px_rgb(0_0_0/0.55)]"
    >
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
        <p className="mb-3 text-[13px] font-medium tracking-[-0.01em]">{mode === "draft" ? "Filter All" : `${initialName}: what it shows`}</p>
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
          <button onClick={onClose} className="h-7 rounded-md px-3 text-[12px] text-muted hover:bg-panel-2 hover:text-fg">
            Done
          </button>
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
      {mode === "draft" && (
        <SaveDraft
          onSave={(n) => onSave(n, f)}
          disabled={isEmptyFilter(f)}
        />
      )}
    </div>
  );
}

/** On All: name what's filtered and keep it as a view. */
function SaveDraft({ onSave, disabled }: { onSave: (name: string) => void; disabled: boolean }) {
  const [name, setName] = useState("");
  return (
    <form
      className="mt-2 flex items-center gap-1.5"
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
  // The cards' floating toolbar (`HoverActions` in nodes.tsx), so the two read as one.
  return (
    <div className="group/slot relative">
      {children}
      <div
        className="pointer-events-none absolute -top-3.5 right-2.5 z-10 flex translate-y-1 items-center gap-0.5 rounded-lg border border-line-2 p-0.5 text-muted opacity-0 shadow-pop inset-shadow-highlight glass transition-[opacity,translate] duration-150 ease-out group-hover/slot:pointer-events-auto group-hover/slot:translate-y-0 group-hover/slot:opacity-100 focus-within:pointer-events-auto focus-within:translate-y-0 focus-within:opacity-100 motion-reduce:transition-none"
        data-nodrag
        data-touch-tools
      >
        {items.map((i) => {
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
        {extra}
      </div>
    </div>
  );
}
