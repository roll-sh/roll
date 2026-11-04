"use client";

import { Bookmark, ChevronDown, Pencil, Pin as PinIcon, PinOff, Plus, Search, SlidersHorizontal, Trash2, X, ArrowLeft, ArrowRight, FileDiff as DiffIcon } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import type { AgentsServices } from "@/lib/agents-services";
import type { AgentsData } from "@/lib/agents-store";
import {
  addView,
  ALL,
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
  setViewFilter,
  setViewOrder,
  STATUS_FILTERS,
  statusFilterLabel,
  unpin,
  viewById,
  viewsOf,
  type CanvasDoc,
  type Pin,
  type PinKind,
  type ViewFilter,
  type ViewLike,
} from "@/lib/agents-views";
import type { Order } from "@/lib/agents-layout";
import { Menu } from "./menu";

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
        const next = addView(d, name, f, vid);
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
  const togglePin = useCallback(
    (kind: PinKind, target: string, exists?: (p: Pin) => boolean) =>
      update((d) => (isPinned(viewById(d, id), kind, target) ? unpin(d, id, kind, target) : pin(d, id, kind, target, exists))),
    [id, update],
  );
  const resize = useCallback((p: Pin, w: number, h: number) => update((d) => resizePin(d, id, p.kind, p.id, w, h)), [id, update]);
  const views = useMemo(() => viewsOf(doc), [doc]);
  return { doc, views, current, filter, select, setFilter, saveAs, update, setOrder, togglePin, resize };
}

const typing = (el: Element | null) => !!el && ((el as HTMLElement).isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName) || !!el.closest(".xterm"));

/**
 * The views bar over the canvas: All and the owner's views as tabs (with how many threads each
 * picks), a filter field and the filter editor, and saving what's filtered as a view. Keys (not
 * while typing): ⌥1…⌥9 show a view, [ and ] the previous and next, / filters, ⌥S saves the
 * filter as a view.
 */
export function ViewsBar({ views: v, data, active, phone }: { views: CanvasViews; data: AgentsData; active: boolean; phone: boolean }) {
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
      else return;
      e.preventDefault();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [active, views, select, v.current.id]);

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
                }
              }}
              placeholder={onAll ? "Filter threads…  /" : `Filter ${v.current.name}…`}
              aria-label="Filter threads"
              className="min-w-0 flex-1 bg-transparent text-[12.5px] text-fg outline-none placeholder:text-subtle"
            />
          </label>
        )}
        <button
          onClick={() => setEditor((e) => (e ? null : { mode: "edit" }))}
          aria-expanded={!!editor}
          aria-label={onAll ? "Filters" : `${v.current.name} filters`}
          title="Filters"
          className={`flex h-7 items-center gap-1 rounded-md px-2 text-[12px] transition-colors duration-150 hover:bg-fg/[0.05] hover:text-fg ${filters ? "text-fg" : "text-subtle"} ${editor ? "bg-fg/[0.07]" : ""}`}
        >
          <SlidersHorizontal className="size-3.5" strokeWidth={1.75} />
          {filters > 0 && <span className="font-mono text-[10.5px] text-accent tabular-nums">{filters}</span>}
        </button>
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
  const save = () => name.trim() && onSave(name, f);

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
            <button onClick={save} disabled={!name.trim()} className="h-7 rounded-md bg-fg px-3 text-[12px] font-medium text-bg hover:opacity-85 disabled:opacity-40">
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
  children,
}: {
  view: ViewLike;
  items: { kind: PinKind; id: string; label: string }[];
  onToggle: (kind: PinKind, id: string) => void;
  children: ReactNode;
}) {
  return (
    <div className="group/slot relative">
      {children}
      <div
        className="absolute -top-3 right-3 z-10 flex items-center gap-0.5 rounded-md border border-line bg-panel p-0.5 text-subtle opacity-0 shadow-[0_4px_14px_-6px_rgb(0_0_0/0.5)] transition-opacity duration-150 group-hover/slot:opacity-100 focus-within:opacity-100 [@media(hover:none)]:opacity-100"
        data-nodrag
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
              className={`grid size-6 place-items-center rounded transition hover:bg-panel-2 hover:text-fg [&>svg]:size-3.5 ${pinned ? "text-accent" : ""}`}
            >
              {i.kind === "diff" ? <DiffIcon /> : pinned ? <PinOff /> : <PinIcon />}
            </button>
          );
        })}
      </div>
    </div>
  );
}
