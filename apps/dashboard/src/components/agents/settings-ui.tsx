"use client";

/**
 * T3 Code's settings layout (`settingsLayout.tsx`, `SettingsPanels.tsx` there, MIT) for the side
 * panel: a search box over sections; a section is a title over a grouped card; a row is its title
 * and description on the left and its control on the right (stacked when narrow), with a reset
 * button when it differs from its default. Searching keeps the rows that match (T3's
 * `searchSettings`) in their sections and hides the rest.
 */

import { RotateCcw, Search, X } from "lucide-react";
import { createContext, useContext, useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { searchSettings, type SettingItem } from "@/lib/settings-search";
import { IconButton } from "./session";
import { cn } from "./ui";

/** A row's words: its title, its section, its description, other terms. */
export interface Setting extends SettingItem {
  description?: string;
}

const SearchCtx = createContext<{ query: string; matches: Set<string> | null }>({ query: "", matches: null });

/** Whether a row (or any of `ids`) shows with the current search. */
export function useShown(ids: readonly string[]): boolean {
  const { matches } = useContext(SearchCtx);
  return !matches || ids.some((id) => matches.has(id));
}

/** The search's words, to open what holds a match (a provider's details). */
export const useSearching = () => useContext(SearchCtx).query.trim().length > 0;

export function SettingsPage({
  icon,
  title,
  subtitle,
  onClose,
  items,
  search = true,
  children,
}: {
  icon: ReactNode;
  title: string;
  subtitle: string;
  onClose: () => void;
  /** Every row the page has, for the search. */
  items: readonly Setting[];
  /** Offer the search box (a short page needs none). */
  search?: boolean;
  children: ReactNode;
}) {
  const [query, setQuery] = useState("");
  const box = useRef<HTMLInputElement>(null);
  const matches = useMemo(() => (query.trim() ? new Set(searchSettings(query, items).map((i) => i.id)) : null), [query, items]);
  // T3: `/` or ⌘/Ctrl+F searches the settings.
  useEffect(() => {
    if (!search) return;
    const onKey = (e: KeyboardEvent) => {
      const el = e.target as HTMLElement | null;
      const typing = el && (el.isContentEditable || /^(INPUT|TEXTAREA|SELECT)$/.test(el.tagName));
      if ((e.key === "/" && !typing) || ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "f")) {
        e.preventDefault();
        box.current?.focus();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [search]);
  return (
    <div className="flex min-h-0 flex-1 flex-col">
      <div className="flex h-12 shrink-0 items-center gap-2.5 border-b border-line px-3">
        {icon}
        <div className="min-w-0 flex-1 leading-tight">
          <p className="truncate text-[13px] font-medium tracking-[-0.01em]">{title}</p>
          <p className="text-[11px] text-subtle">{subtitle}</p>
        </div>
        <IconButton label="Close (Esc)" onClick={onClose}>
          <X className="size-4" />
        </IconButton>
      </div>
      <div className={cn("shrink-0 px-4 pt-3 pb-1", !search && "hidden")}>
        <label className="flex h-8 items-center gap-2 rounded-lg border border-line bg-panel px-2.5 text-[13px] focus-within:border-line-2">
          <Search className="size-3.5 shrink-0 text-subtle" aria-hidden />
          <input
            ref={box}
            type="search"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            onKeyDown={(e) => {
              if (e.key === "Escape" && query) {
                e.preventDefault();
                e.stopPropagation();
                setQuery("");
              }
            }}
            placeholder="Search settings"
            aria-label="Search settings"
            className="min-w-0 flex-1 bg-transparent outline-none placeholder:text-subtle [&::-webkit-search-cancel-button]:hidden"
          />
          {query && (
            <button type="button" onClick={() => setQuery("")} className="text-subtle hover:text-fg" aria-label="Clear the search">
              <X className="size-3.5" />
            </button>
          )}
        </label>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-4 pt-3 pb-8" data-scroll>
        <SearchCtx.Provider value={{ query, matches }}>
          {matches && matches.size === 0 ? <p className="py-10 text-center text-[13px] text-subtle">No settings found</p> : children}
        </SearchCtx.Provider>
      </div>
    </div>
  );
}

/** A titled group of rows; hidden while a search matches none of `ids`. */
export function SettingsSection({ title, ids, action, children }: { title: string; ids: readonly string[]; action?: ReactNode; children: ReactNode }) {
  if (!useShown(ids)) return null;
  return (
    <section className="mb-6" aria-label={title}>
      <div className="mb-2 flex items-center gap-2 px-0.5">
        <h3 className="flex-1 text-[12px] font-medium text-muted">{title}</h3>
        {action}
      </div>
      <div className="divide-y divide-line overflow-hidden rounded-xl border border-line bg-panel">{children}</div>
    </section>
  );
}

/**
 * One setting: title and description, the control at the end, a reset when it was changed
 * (`onReset`). `children` go under it (lists, editors). Hidden while a search doesn't match it.
 */
export function SettingRow({
  s,
  control,
  onReset,
  resetLabel = "Reset to default",
  note,
  children,
  className,
}: {
  s: Setting;
  control?: ReactNode;
  onReset?: (() => void) | null;
  resetLabel?: string;
  /** A quiet marker after the title ("from config.toml"). */
  note?: ReactNode;
  children?: ReactNode;
  className?: string;
}) {
  if (!useShown([s.id])) return null;
  return (
    <div className={cn("px-3.5 py-3 text-[13px]", className)} data-setting={s.id}>
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2">
        <div className="min-w-48 flex-1">
          <div className="flex items-center gap-1.5">
            <h4 className="font-medium tracking-[-0.01em]">{s.title}</h4>
            {note}
            {onReset && (
              <button type="button" onClick={onReset} title={resetLabel} aria-label={`${resetLabel}: ${s.title}`} className="grid size-5 place-items-center rounded text-subtle transition hover:bg-fg/[0.06] hover:text-fg">
                <RotateCcw className="size-3" />
              </button>
            )}
          </div>
          {s.description && <p className="mt-0.5 text-[12px] leading-snug text-subtle">{s.description}</p>}
        </div>
        {control !== undefined && <div className="flex min-w-0 shrink-0 items-center gap-2">{control}</div>}
      </div>
      {children}
    </div>
  );
}

/** A quiet tag after a row's title. */
export function Note({ children, title }: { children: ReactNode; title?: string }) {
  return (
    <span className="rounded border border-line px-1 py-px text-[10.5px] text-subtle" title={title}>
      {children}
    </span>
  );
}

export const field = "h-8 min-w-0 rounded-lg border border-line-2 bg-bg px-2.5 text-[13px] text-fg outline-none placeholder:text-subtle hover:border-subtle focus:border-subtle disabled:opacity-60";

/**
 * A text setting saved when it's left or Enter is pressed (T3's inputs), shown as `value` until
 * edited; an empty one is unset (the placeholder says what applies then). Escape puts it back.
 */
export function TextField({
  label,
  value,
  placeholder,
  onSave,
  disabled,
  mono = false,
  className,
  type = "text",
}: {
  label: string;
  value: string;
  placeholder?: string;
  onSave: (value: string) => Promise<unknown> | void;
  disabled?: boolean;
  mono?: boolean;
  className?: string;
  type?: "text" | "email";
}) {
  const [draft, setDraft] = useState(value);
  useEffect(() => setDraft(value), [value]);
  const commit = () => {
    if (draft.trim() !== value.trim()) void onSave(draft.trim());
  };
  return (
    <input
      type={type}
      aria-label={label}
      value={draft}
      placeholder={placeholder}
      disabled={disabled}
      spellCheck={false}
      onChange={(e) => setDraft(e.target.value)}
      onBlur={commit}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          commit();
        } else if (e.key === "Escape" && draft !== value) {
          e.preventDefault();
          e.stopPropagation();
          setDraft(value);
        }
      }}
      className={cn(field, mono && "font-mono text-[12px]", className ?? "w-64 max-w-full")}
    />
  );
}

/** What saving is doing, for a page's footer line. */
export function SaveState({ saving, error, offline }: { saving: boolean; error: string | null; offline?: string | null }) {
  if (offline) return <p className="mb-4 rounded-lg border border-line bg-panel px-3 py-2 text-[12px] text-subtle">{offline}</p>;
  if (!saving && !error) return null;
  return (
    <p role={error ? "alert" : "status"} className={cn("mb-4 rounded-lg border px-3 py-2 text-[12px]", error ? "border-danger/40 text-danger" : "border-line text-subtle")}>
      {error ?? <span className="shimmer">Saving…</span>}
    </p>
  );
}
