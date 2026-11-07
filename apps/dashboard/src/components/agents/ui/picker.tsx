"use client";

/**
 * The picker every field that names a thing uses (docs/code/agents-design.md, "Pickers"): never
 * type an id. A field-shaped trigger shows what's picked by name (a chip each when it holds a
 * list); it opens a searchable list of the real things with their pictures and colours, recents
 * first, keyboard throughout. The value stays the thing's stable id.
 *
 * - Rows come from `options` (filtered here) or a `source` (searched on the server, paged: Linear's
 *   teams, Slack's channels…); `extras` are rows of the field's own (`roll`, `No one`, a kind of
 *   state) shown first.
 * - A stored value nobody listed yet is resolved through `source.load({ ids })`; one that resolves
 *   to nothing still shows, as itself.
 * - `custom` takes what was typed ("Use “…”"); `onCreate` makes a new one ("Create label “…”");
 *   `variables` adds "Use a variable" (automation fields: the value becomes `{{ path }}`).
 * - A source's `problem` ("Reconnect Linear") shows quietly at the bottom; typing still works.
 * - Like every popup here it closes on a press outside, Escape or a pick, never on a blur alone
 *   (Safari doesn't focus a clicked row). A list stays open while picking several.
 */

import { Popover as PopoverPrimitive } from "@base-ui/react/popover";
import { Bot, Braces, Check, ChevronDown, Circle, CircleCheck, CircleDashed, CircleDot, CircleX, FolderGit2, GitBranch, GitPullRequest, Hash, Lock, Milestone, Plus, Server, Shield, Tag, TextCursorInput, TriangleAlert, Users, Workflow, X } from "lucide-react";
import { useCallback, useEffect, useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import { Combobox, ComboboxItem, ComboboxList, ComboboxSearchInput } from "./combobox";
import { cn, popupMotion } from "./cn";
import { useLayer } from "./layer";

export interface PickerOption {
  id: string;
  label: string;
  /** Another value that names the same row (a team's key): a stored key shows this row. */
  key?: string;
  hint?: string;
  group?: string;
  avatar?: string;
  color?: string;
  /** A kind (`channel`, `private`, `user`, `group`, a Linear state's type…) or an element. */
  icon?: string | ReactNode;
}

export interface PickerProblem {
  text: string;
  action?: { label: string; href?: string; onClick?: () => void };
}

export interface PickerPage {
  items: PickerOption[];
  next?: string;
  problem?: PickerProblem | null;
}

/** Rows looked up somewhere else. `key` names the list (it changes with the rows' parent). */
export interface PickerSource {
  key: string;
  load(q: { q: string; cursor?: string; ids?: readonly string[] }): Promise<PickerPage>;
}

export interface PickerVariable {
  path: string;
  group: string;
  hint?: string;
}

interface Common {
  /** The field's name: the trigger's and the list's accessible name. */
  label: string;
  /** What the trigger says while nothing is picked. */
  placeholder?: string;
  /** What the search says. */
  searchPlaceholder?: string;
  options?: readonly PickerOption[];
  source?: PickerSource;
  extras?: readonly PickerOption[];
  /** The heading over `extras`. */
  extrasGroup?: string;
  /** Take a typed value nothing matches. */
  custom?: boolean;
  /** Make a new one from what was typed; `noun` says what ("label"). */
  onCreate?: (name: string) => Promise<PickerOption>;
  noun?: string;
  /** Template variables the value may be (`{{ path }}`). */
  variables?: readonly PickerVariable[];
  /** Remember picks under this name (per device) and show them first. */
  recents?: string;
  disabled?: boolean;
  error?: string;
  /** Its size where it sits (`w-48`, `min-w-0 flex-1`); full width when not given. */
  className?: string;
  /** Show ids in mono (a value that's an id, not a name). */
  mono?: boolean;
}

export type PickerProps = Common &
  (
    | { multiple?: false; value: string | null; onChange: (value: string | null) => void; clearable?: boolean }
    | { multiple: true; value: readonly string[]; onChange: (value: string[]) => void; clearable?: never }
  );

const isTemplate = (v: string) => v.includes("{{");
const norm = (s: string) => s.trim().toLowerCase();
const RECENTS = 5;
const DEBOUNCE_MS = 180;

/** Whether `o` answers a stored value (its id, or its key in any case). */
const answers = (o: PickerOption, v: string) => o.id === v || (o.key !== undefined && norm(o.key) === norm(v));
const matches = (o: PickerOption, q: string) => !q || [o.label, o.key, o.hint].some((s) => s && norm(s).includes(q));

// ── recents (per device) ──

const recentsKey = (name: string) => `roll:picker:recents:${name}`;

function readRecents(name: string | undefined): PickerOption[] {
  if (!name || typeof localStorage === "undefined") return [];
  try {
    const v: unknown = JSON.parse(localStorage.getItem(recentsKey(name)) ?? "[]");
    return Array.isArray(v) ? v.filter((o): o is PickerOption => !!o && typeof o.id === "string" && typeof o.label === "string").slice(0, RECENTS) : [];
  } catch {
    return [];
  }
}

function remember(name: string | undefined, o: PickerOption) {
  if (!name || typeof localStorage === "undefined" || typeof o.icon === "object") return;
  try {
    const next = [o, ...readRecents(name).filter((r) => r.id !== o.id)].slice(0, RECENTS);
    localStorage.setItem(recentsKey(name), JSON.stringify(next));
  } catch {
    // Storage full or off: recents are a nicety.
  }
}

// ── loading a source ──

interface Loaded {
  items: PickerOption[];
  next?: string;
  problem: PickerProblem | null;
  loading: boolean;
}

/** The source's rows for `q` (debounced), with "load more". */
function useSourceRows(source: PickerSource | undefined, q: string, active: boolean) {
  const [state, setState] = useState<Loaded>({ items: [], problem: null, loading: false });
  const run = useRef(0);
  const fetchPage = useCallback(
    async (cursor?: string) => {
      if (!source) return;
      const mine = ++run.current;
      setState((s) => ({ ...s, loading: true }));
      try {
        const page = await source.load({ q, cursor });
        if (mine !== run.current) return;
        setState((s) => ({ items: cursor ? [...s.items, ...page.items] : page.items, next: page.next, problem: page.problem ?? null, loading: false }));
      } catch (e) {
        if (mine !== run.current) return;
        setState((s) => ({ ...s, problem: { text: e instanceof Error ? e.message : String(e) }, loading: false }));
      }
    },
    [source, q],
  );
  useEffect(() => {
    if (!active || !source) return;
    const t = setTimeout(() => void fetchPage(), q ? DEBOUNCE_MS : 0);
    return () => clearTimeout(t);
  }, [active, source, q, fetchPage]);
  return { ...state, more: () => state.next && void fetchPage(state.next) };
}

/** Rows for stored values nobody has listed yet. */
function useResolved(source: PickerSource | undefined, values: readonly string[], known: (v: string) => boolean) {
  const [found, setFound] = useState<Map<string, PickerOption>>(new Map());
  // Asked and answered (found or not): until then a value isn't shown as typed.
  const [settled, setSettled] = useState<ReadonlySet<string>>(new Set());
  const asked = useRef(new Set<string>());
  // Answers are kept until the picker goes away: marking values asked changes `want`, and a
  // cleanup tied to it dropped every answer (a typed value never settled, a real one never resolved).
  const mounted = useRef(true);
  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
    };
  }, []);
  const missing = values.filter((v) => !isTemplate(v) && !known(v) && !found.has(v) && !asked.current.has(`${source?.key}\u0000${v}`));
  const want = missing.join("\u0000");
  useEffect(() => {
    if (!source || !want) return;
    const ids = want.split("\u0000");
    for (const v of ids) asked.current.add(`${source.key}\u0000${v}`);
    source
      .load({ q: "", ids })
      .then((p) => {
        if (!mounted.current) return;
        setFound((f) => {
          const next = new Map(f);
          for (const v of ids) {
            const hit = p.items.find((o) => answers(o, v));
            if (hit) next.set(v, hit);
          }
          return next;
        });
      })
      .catch(() => {})
      .finally(() => mounted.current && setSettled((s) => new Set([...s, ...ids])));
  }, [source, want]);
  return { found, pending: (v: string) => !!source && !isTemplate(v) && !known(v) && !found.has(v) && !settled.has(v) };
}

// ── the picker ──

type Row =
  | { kind: "option"; key: string; option: PickerOption; group: string | null }
  | { kind: "custom" | "create"; key: string; text: string }
  | { kind: "variables" | "more" | "none"; key: string }
  | { kind: "variable"; key: string; variable: PickerVariable };

export function Picker(props: PickerProps) {
  const { label, placeholder, searchPlaceholder, options, source, extras = [], extrasGroup, custom, onCreate, noun, variables, recents, disabled, error, className, mono } = props;
  const values: readonly string[] = props.multiple ? props.value : props.value ? [props.value] : [];
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [mode, setMode] = useState<"rows" | "variables">("rows");
  const [creating, setCreating] = useState(false);
  const [createError, setCreateError] = useState<string | null>(null);
  const [seen, setSeen] = useState<Map<string, PickerOption>>(new Map());
  const search = useRef<HTMLInputElement>(null);
  /** The row the keyboard is on (Enter picks it, as T3's pickers do). */
  const highlighted = useRef<string | null>(null);
  const q = norm(query);
  const remote = useSourceRows(source, query.trim(), open && mode === "rows");
  const recent = useMemo(() => (open ? readRecents(recents) : []), [open, recents]);

  // Every row seen so far, to name picked values.
  useEffect(() => {
    if (!remote.items.length) return;
    setSeen((m) => {
      const next = new Map(m);
      for (const o of remote.items) next.set(o.id, o);
      return next;
    });
  }, [remote.items]);
  const local = useMemo(() => [...extras, ...(options ?? [])], [extras, options]);
  const lookupKnown = useCallback((v: string) => local.some((o) => answers(o, v)) || [...seen.values()].some((o) => answers(o, v)), [local, seen]);
  const resolved = useResolved(source, values, lookupKnown);
  const optionFor = (v: string): PickerOption | null => local.find((o) => answers(o, v)) ?? [...seen.values()].find((o) => answers(o, v)) ?? resolved.found.get(v) ?? null;

  const isPicked = (o: PickerOption) => values.some((v) => answers(o, v));

  const rows: Row[] = useMemo(() => {
    if (mode === "variables") {
      const vs = (variables ?? []).filter((v) => !q || norm(v.path).includes(q) || norm(v.group).includes(q));
      return vs.slice(0, 60).map((v) => ({ kind: "variable" as const, key: `var:${v.path}`, variable: v }));
    }
    const out: Row[] = [];
    const shown = new Set<string>();
    const push = (o: PickerOption, group: string | null) => {
      if (shown.has(o.id)) return;
      shown.add(o.id);
      out.push({ kind: "option", key: `opt:${o.id}`, option: o, group });
    };
    if (props.clearable && !props.multiple && props.value && !q) out.push({ kind: "none", key: "none" });
    if (!q) for (const r of recent) if (!extras.some((e) => e.id === r.id)) push(r, "Recent");
    for (const e of extras) if (matches(e, q)) push(e, extrasGroup ?? null);
    const pool = source ? remote.items : (options ?? []).filter((o) => matches(o, q));
    for (const o of pool) push(o, o.group ?? null);
    const exact = [...extras, ...pool].some((o) => norm(o.label) === q || (o.key && norm(o.key) === q));
    if (q && !exact && onCreate) out.push({ kind: "create", key: "create", text: query.trim() });
    if (q && !exact && custom) out.push({ kind: "custom", key: "custom", text: query.trim() });
    if (source && remote.next) out.push({ kind: "more", key: "more" });
    if (variables?.length && !q) out.push({ kind: "variables", key: "variables" });
    return out;
  }, [mode, variables, q, recent, extras, extrasGroup, source, remote.items, remote.next, options, onCreate, custom, query, props.clearable, props.multiple, props.value]);
  const leads = rows.some((r) => r.kind === "option" && hasLead(r.option));

  const reset = () => {
    setQuery("");
    setMode("rows");
    setCreateError(null);
  };
  const close = () => {
    setOpen(false);
    reset();
  };

  const pickValue = (v: string, o?: PickerOption) => {
    if (o) remember(recents, o);
    if (props.multiple) {
      const has = props.value.some((x) => x === v || (o && answers(o, x)));
      props.onChange(has ? props.value.filter((x) => !(x === v || (o && answers(o, x)))) : [...props.value, v]);
      setQuery("");
      search.current?.focus();
    } else {
      props.onChange(v);
      close();
    }
  };

  const choose = async (key: string) => {
    const row = rows.find((r) => r.key === key);
    if (!row) return;
    switch (row.kind) {
      case "option":
        return pickValue(row.option.id, row.option);
      case "custom":
        return pickValue(row.text);
      case "variable":
        setMode("rows");
        return pickValue(`{{ ${row.variable.path} }}`);
      case "variables":
        setQuery("");
        setMode("variables");
        search.current?.focus();
        return;
      case "more":
        remote.more();
        return;
      case "none":
        if (!props.multiple) props.onChange(null);
        close();
        return;
      case "create": {
        if (!onCreate || creating) return;
        setCreating(true);
        setCreateError(null);
        try {
          const made = await onCreate(row.text);
          setSeen((m) => new Map(m).set(made.id, made));
          pickValue(made.id, made);
        } catch (e) {
          setCreateError(e instanceof Error ? e.message : String(e));
        } finally {
          setCreating(false);
        }
      }
    }
  };

  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter" && !e.nativeEvent.isComposing) {
      // Nothing highlighted yet (just opened, nothing typed): Enter takes the first row, as T3's.
      const key = highlighted.current && rows.some((r) => r.key === highlighted.current) ? highlighted.current : rows.find((r) => r.kind !== "none" && r.kind !== "more")?.key;
      if (key) {
        (e as typeof e & { preventBaseUIHandler?: () => void }).preventBaseUIHandler?.();
        e.preventDefault();
        void choose(key);
        return;
      }
    }
    if (e.key === "Backspace" && !query) {
      if (mode === "variables") {
        e.preventDefault();
        setMode("rows");
      } else if (props.multiple && props.value.length) {
        e.preventDefault();
        props.onChange(props.value.slice(0, -1));
      }
    }
  };

  const listId = useId();
  const problem = mode === "rows" ? remote.problem : null;
  const keys = rows.map((r) => r.key);
  const emptyText = mode === "variables" ? "No variables match" : remote.loading ? null : problem ? null : q ? "Nothing matches" : "Nothing here yet";

  const layer = useLayer();
  return (
    <PopoverPrimitive.Root
      open={open}
      onOpenChange={(next) => {
        if (disabled) return;
        setOpen(next);
        if (!next) reset();
      }}
    >
      <div className={cn("relative", className ?? "w-full")} data-picker>
        {props.multiple ? (
          <div className={cn(field, "flex min-h-8 flex-wrap items-center gap-1 py-1 ps-1", error && "border-danger/50", disabled && "opacity-60")}>
            {props.value.map((v, i) => (
              <ValueChip key={`${v}:${i}`} value={v} option={optionFor(v)} pending={resolved.pending(v)} mono={mono} onRemove={disabled ? undefined : () => props.onChange(props.value.filter((_, j) => j !== i))} />
            ))}
            <PopoverPrimitive.Trigger
              disabled={disabled}
              aria-label={props.value.length ? `Add to ${label}` : label}
              className="flex h-6 min-w-24 flex-1 cursor-pointer items-center gap-1 rounded-md px-1.5 text-left text-[12.5px] text-subtle outline-none hover:text-fg focus-visible:text-fg"
            >
              {props.value.length ? <Plus aria-hidden className="size-3.5" /> : null}
              <span className="truncate">{props.value.length ? "Add" : (placeholder ?? `Pick ${label.toLowerCase()}`)}</span>
            </PopoverPrimitive.Trigger>
          </div>
        ) : (
          <PopoverPrimitive.Trigger
            disabled={disabled}
            aria-label={label}
            className={cn(field, "flex min-h-8 w-full cursor-pointer items-center gap-2 pe-7 text-left outline-none data-disabled:opacity-60", error && "border-danger/50", props.clearable && props.value && "pe-12")}
          >
            {props.value ? <OneValue value={props.value} option={optionFor(props.value)} pending={resolved.pending(props.value)} mono={mono} /> : <span className="truncate text-subtle">{placeholder ?? `Pick ${label.toLowerCase()}`}</span>}
            <ChevronDown aria-hidden className="pointer-events-none absolute end-2.5 size-3.5 text-subtle" />
          </PopoverPrimitive.Trigger>
        )}
        {!props.multiple && props.clearable && props.value && !disabled && (
          <button type="button" aria-label={`Clear ${label.toLowerCase()}`} onClick={() => props.onChange(null)} className="absolute end-7 top-1/2 grid size-5 -translate-y-1/2 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg">
            <X className="size-3" />
          </button>
        )}
      </div>
      {error && <p className="mt-1 text-[11.5px] text-danger">{error}</p>}
      <PopoverPrimitive.Portal {...layer}>
        <PopoverPrimitive.Positioner className="z-[130]" side="bottom" align="start" sideOffset={4}>
          <PopoverPrimitive.Popup
            data-picker-popup
            aria-label={label}
            // Opened by touch, the popup takes focus (Base UI's default) so a phone's keyboard stays down until the search is tapped.
            initialFocus={(type) => (type === "touch" ? true : search.current)}
            className={cn("rounded-lg text-fg outline-none", popupMotion)}
          >
            {/* Solid, not glass: a list of names must read over a busy form (glass lets the fields behind show through where a browser doesn't blur). */}
            <div className={cn("flex max-h-[min(24rem,var(--available-height))] w-[max(18rem,var(--anchor-width))] max-w-[min(26rem,var(--available-width))] flex-col rounded-lg border border-outline bg-popover shadow-pop inset-shadow-highlight")}>
            <Combobox<string>
              inline
              open
              items={keys}
              filteredItems={keys}
              filter={null}
              autoHighlight
              value={null}
              onItemHighlighted={(k) => (highlighted.current = typeof k === "string" ? k : null)}
              onValueChange={(k) => {
                if (typeof k === "string") void choose(k);
              }}
            >
              <ComboboxSearchInput
                ref={search}
                aria-label={mode === "variables" ? "Search variables" : `Search ${label.toLowerCase()}`}
                aria-controls={listId}
                placeholder={mode === "variables" ? "Search variables…" : (searchPlaceholder ?? (source || (options?.length ?? 0) > 8 ? "Search…" : custom ? "Search or type…" : "Search…"))}
                value={query}
                onChange={(e) => setQuery(e.target.value)}
                onKeyDown={onSearchKey}
              />
              {mode === "variables" && (
                <button type="button" onClick={() => (setMode("rows"), setQuery(""), search.current?.focus())} className="mx-3 mt-1.5 self-start text-[11.5px] text-subtle hover:text-fg">
                  ← Back to the list
                </button>
              )}
              <ComboboxList id={listId} aria-label={label} className="max-h-72">
                {rows.map((r, i) => {
                  const heading = r.kind === "option" && r.group && (i === 0 || rows[i - 1].kind !== "option" || (rows[i - 1] as { group: string | null }).group !== r.group) ? r.group : r.kind === "variable" && (i === 0 || (rows[i - 1] as { variable?: PickerVariable }).variable?.group !== r.variable.group) ? r.variable.group : null;
                  return (
                    <div key={r.key} role="presentation">
                      {heading && <div className="px-2 pt-2 pb-1 text-[11px] font-medium text-subtle">{heading}</div>}
                      <RowView row={r} picked={r.kind === "option" && isPicked(r.option)} multiple={!!props.multiple} noun={noun} creating={creating} mono={mono} pad={leads} />
                    </div>
                  );
                })}
              </ComboboxList>
              {emptyText && rows.length === 0 && <p className="px-3 py-5 text-center text-[12.5px] text-subtle">{emptyText}</p>}
              {remote.loading && mode === "rows" && <p className="shimmer px-3 py-2 text-[12px] text-subtle">Loading…</p>}
              {(problem || createError) && (
                <div className="flex items-center gap-2 border-t border-line px-3 py-2 text-[12px] text-muted" data-picker-problem>
                  <TriangleAlert aria-hidden className="size-3.5 shrink-0 text-subtle" />
                  <span className="min-w-0 flex-1">{createError ?? problem!.text}</span>
                  {!createError && problem!.action && (
                    <a
                      href={problem!.action.href ?? "#"}
                      onClick={(e) => {
                        if (problem!.action!.onClick) {
                          e.preventDefault();
                          problem!.action!.onClick();
                        }
                        close();
                      }}
                      className="shrink-0 text-accent hover:underline"
                    >
                      {problem!.action.label}
                    </a>
                  )}
                </div>
              )}
              {props.multiple && props.value.length > 0 && (
                <div className="flex items-center justify-between border-t border-line px-3 py-1.5 text-[11.5px] text-subtle">
                  <span>{props.value.length} picked</span>
                  <button type="button" onClick={close} className="rounded px-1.5 py-0.5 text-fg hover:bg-fg/[0.06]">
                    Done
                  </button>
                </div>
              )}
            </Combobox>
            </div>
          </PopoverPrimitive.Popup>
        </PopoverPrimitive.Positioner>
      </PopoverPrimitive.Portal>
    </PopoverPrimitive.Root>
  );
}

const field = "relative rounded-lg border border-line-2 bg-panel-2 px-2.5 text-[13px] text-fg transition-colors hover:border-line-3 focus-within:border-line-3";

function RowView({ row, picked, multiple, noun, creating, mono, pad }: { row: Row; picked: boolean; multiple: boolean; noun?: string; creating: boolean; mono?: boolean; pad: boolean }) {
  switch (row.kind) {
    case "option": {
      const o = row.option;
      return (
        <ComboboxItem value={row.key} data-picked={picked || undefined} className={cn("w-full", picked && "bg-fg/[0.04]")}>
          {/* Where some rows lead with a picture or icon, the rest keep its room, so the names line up. */}
          {hasLead(o) ? <Lead option={o} /> : pad && <span aria-hidden className="size-4 shrink-0" />}
          <span className={cn("min-w-0 flex-1 truncate", mono && o.label === o.id && "font-mono text-[12px]")}>{o.label}</span>
          {o.hint && <span className="max-w-[45%] shrink-0 truncate text-[11.5px] text-subtle">{o.hint}</span>}
          {multiple ? (
            <span aria-hidden className={cn("grid size-3.5 shrink-0 place-items-center rounded-[4px] border", picked ? "border-accent bg-accent text-bg" : "border-line-3")}>
              {picked && <Check className="size-2.5" strokeWidth={3} />}
            </span>
          ) : (
            // The check keeps its room on every row, so hints stay flush right.
            <Check aria-hidden className={cn("size-3.5 shrink-0 text-fg", !picked && "invisible")} />
          )}
        </ComboboxItem>
      );
    }
    case "custom":
      return (
        <ComboboxItem value={row.key} className="w-full text-muted data-highlighted:text-fg">
          <TextCursorInput aria-hidden className="size-3.5" />
          <span className="min-w-0 flex-1 truncate">
            Use <span className="font-mono text-[12px] text-fg">{row.text}</span> as typed
          </span>
          <span className="shrink-0 text-[11px] text-subtle">not in the list</span>
        </ComboboxItem>
      );
    case "create":
      return (
        <ComboboxItem value={row.key} disabled={creating} className="w-full text-muted data-highlighted:text-fg">
          <Plus aria-hidden className="size-3.5" />
          <span className={cn("min-w-0 flex-1 truncate", creating && "shimmer")}>
            {creating ? "Creating…" : <>Create {noun ?? "new"} <span className="text-fg">“{row.text}”</span></>}
          </span>
        </ComboboxItem>
      );
    case "variables":
      return (
        <ComboboxItem value={row.key} className="w-full text-muted data-highlighted:text-fg">
          <Braces aria-hidden className="size-3.5" />
          <span className="min-w-0 flex-1">Use a variable</span>
        </ComboboxItem>
      );
    case "variable":
      return (
        <ComboboxItem value={row.key} className="w-full">
          <Braces aria-hidden className="size-3.5 text-accent" />
          <span className="min-w-0 flex-1 truncate font-mono text-[12px]">{row.variable.path.replace(/^event\.data\./, "")}</span>
          {row.variable.hint && <span className="max-w-[40%] shrink-0 truncate text-[11.5px] text-subtle">{row.variable.hint}</span>}
        </ComboboxItem>
      );
    case "more":
      return (
        <ComboboxItem value={row.key} className="w-full justify-center text-[12px] text-subtle data-highlighted:text-fg">
          Load more
        </ComboboxItem>
      );
    case "none":
      return (
        <ComboboxItem value={row.key} className="w-full text-muted data-highlighted:text-fg">
          <span className="grid size-4 place-items-center">
            <X aria-hidden className="size-3" />
          </span>
          <span className="min-w-0 flex-1">None</span>
        </ComboboxItem>
      );
  }
}

const STATE_ICONS: Record<string, typeof Circle> = { backlog: CircleDashed, triage: CircleDashed, unstarted: Circle, started: CircleDot, completed: CircleCheck, canceled: CircleX };

/** A row's kind (`icon`) as a lucide icon, in the thing's colour when it has one. */
const KIND_ICONS: Record<string, typeof Circle> = {
  ...STATE_ICONS,
  channel: Hash,
  private: Lock,
  group: Users,
  team: Users,
  role: Shield,
  agent: Bot,
  repo: FolderGit2,
  github: FolderGit2,
  branch: GitBranch,
  label: Tag,
  milestone: Milestone,
  workflow: Workflow,
  environment: Server,
  pull: GitPullRequest,
  issue: CircleDot,
};

/** What leads a row: a picture, a kind's icon in the thing's colour, or its colour as a dot. */
/** Whether a row shows something before its name. */
const hasLead = (o: PickerOption) => !!(o.icon || o.avatar || o.color);

function Lead({ option: o, size = "size-4" }: { option: PickerOption; size?: string }) {
  if (o.icon && typeof o.icon === "object") return <span className={cn("grid shrink-0 place-items-center", size)}>{o.icon}</span>;
  const kind = typeof o.icon === "string" ? o.icon : undefined;
  if (o.avatar || kind === "user")
    return o.avatar ? (
      // eslint-disable-next-line @next/next/no-img-element -- pictures from the integration, any host
      <img src={o.avatar} alt="" referrerPolicy="no-referrer" loading="lazy" className={cn("shrink-0 rounded-full bg-fg/[0.08] object-cover", size)} />
    ) : (
      <span aria-hidden className={cn("grid shrink-0 place-items-center rounded-full bg-fg/[0.08] text-[9px] font-medium text-muted uppercase", size)}>
        {initials(o.label)}
      </span>
    );
  const style = o.color ? { color: o.color } : undefined;
  if (kind && KIND_ICONS[kind]) {
    const Icon = KIND_ICONS[kind];
    return <Icon aria-hidden className="size-3.5 shrink-0 text-subtle" style={style} />;
  }
  if (o.color) return <span aria-hidden className="mx-[3px] size-2.5 shrink-0 rounded-full" style={{ backgroundColor: o.color }} />;
  return null;
}

const initials = (s: string) =>
  s
    .split(/\s+/)
    .filter(Boolean)
    .slice(0, 2)
    .map((w) => w[0])
    .join("");

/** The trigger's picked value: its row's lead and label, a variable, or the stored text as itself. */
function OneValue({ value, option, pending, mono }: { value: string; option: PickerOption | null; pending?: boolean; mono?: boolean }) {
  if (isTemplate(value)) return <span className="min-w-0 truncate font-mono text-[12px] text-accent">{value}</span>;
  // Typed, not picked: its own look (a cursor, mono, dimmer), so it never passes for a real entry.
  if (!option && pending) return <span className={cn("min-w-0 truncate", mono && "font-mono text-[12px]")}>{value}</span>;
  if (!option)
    return (
      <span className="flex min-w-0 items-center gap-1.5 text-soft" title="Typed, not picked from the list">
        <TextCursorInput aria-hidden className="size-3.5 shrink-0 text-subtle" />
        <span className="min-w-0 truncate font-mono text-[12px]">{value}</span>
      </span>
    );
  return (
    <span className="flex min-w-0 flex-1 items-center gap-2">
      <Lead option={option} />
      {/* The name keeps its room; the hint gets what's left. */}
      <span className="min-w-0 max-w-full shrink-0 truncate">{option.label}</span>
      {option.hint && <span className="min-w-0 flex-1 truncate text-[11.5px] text-subtle">{option.hint}</span>}
    </span>
  );
}

function ValueChip({ value, option, pending, mono, onRemove }: { value: string; option: PickerOption | null; pending?: boolean; mono?: boolean; onRemove?: () => void }) {
  const text = option?.label ?? value;
  const typed = !option && !pending && !isTemplate(value);
  return (
    <span
      data-picker-chip
      data-typed={typed || undefined}
      title={typed ? "Typed, not picked from the list" : undefined}
      className={cn(
        "inline-flex h-6 max-w-full items-center gap-1.5 rounded-md ps-1.5 pe-0.5 text-[12px]",
        // A typed value: an outline, no fill, a cursor: it's text, not one of the list's entries.
        typed ? "border border-dashed border-line-3 text-soft" : "bg-fg/[0.06] text-fg",
      )}
    >
      {option ? <Lead option={option} size="size-3.5" /> : typed ? <TextCursorInput aria-hidden className="size-3 shrink-0 text-subtle" /> : null}
      <span className={cn("min-w-0 truncate", (isTemplate(value) || typed || (!option && mono)) && "font-mono text-[11.5px]", isTemplate(value) && "text-accent")}>{text}</span>
      {onRemove && (
        <button type="button" aria-label={`Remove ${text}`} onClick={onRemove} className="grid size-5 place-items-center rounded text-subtle hover:bg-fg/[0.08] hover:text-fg">
          <X className="size-3" />
        </button>
      )}
    </span>
  );
}
