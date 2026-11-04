"use client";

/**
 * The model picker, T3 Code's (MIT) on roll: `ProviderModelPicker`, `ModelPickerContent`,
 * `ModelListRow`, `ModelPickerSidebar` and the docked effort column of `ComposerModelEffort`.
 *
 * The trigger shows the provider, the model and its effort. It opens a popup with a sidebar
 * (Favorites, Recent, then one button per provider, a bar marking the one shown), a search across
 * every provider, the list (favourites first, a star on each row, a check on the picked one), and
 * the picked model's effort levels in a column on the right. Keyboard as T3: type to search,
 * arrows and Enter in the list, ← (empty search) or Shift+Tab to the providers, → to the effort
 * levels, ⌘/Ctrl+Shift+↑/↓ to switch providers, Escape to close. Picking a model with effort
 * levels moves to them; picking a level (or a model without levels) closes it.
 */

import { Asterisk, Braces, Check, ChevronDown, Clock, Hexagon, MousePointer2, PencilLine, ShieldCheck, ShieldOff, Sparkle, Star } from "lucide-react";
import { useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import type { ModelInfo, ProviderInfo, RuntimeMode } from "@/lib/agents-api";
import { modelKey, modelPrefs, parseModelKey, searchModels, sortModels, useModelPrefs, type ModelKey, type PickerModel } from "@/lib/agents-models";
import { cn, Combobox, ComboboxEmpty, ComboboxItem, ComboboxList, ComboboxSearchInput, Menu, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuTrigger, Popover, PopoverPopup, PopoverTrigger } from "./ui";

/** The model a provider runs when none (or an unknown one) is picked: its default, else its first. */
export function resolveModel(info: ProviderInfo | undefined, model: string | null | undefined): ModelInfo | undefined {
  const models = info?.models ?? [];
  return models.find((m) => m.id === model) ?? models.find((m) => m.default) ?? models[0];
}

export interface ModelChoice {
  provider: string;
  model: string | null;
  effort: string | null;
}

type Tab = "favorites" | "recent" | { provider: string };
const sameTab = (a: Tab, b: Tab) => (typeof a === "string" || typeof b === "string" ? a === b : a.provider === b.provider);

const triggerClass =
  "group/model-trigger inline-flex h-7 min-w-0 cursor-pointer items-center gap-1.5 rounded-md px-1.5 text-[12px] text-fg outline-none transition-colors hover:bg-fg/[0.06] focus-visible:ring-2 focus-visible:ring-accent/60 data-popup-open:bg-fg/[0.06] disabled:cursor-not-allowed disabled:opacity-50";

/**
 * The provider, model and effort of a thread, from the machine's installed providers. The owner
 * keeps the choice: `onChange` gets the whole of it (a new model clears the effort unless one is
 * picked with it).
 */
export function ModelPicker({
  providers,
  value,
  onChange,
  side = "top",
  disabled = false,
}: {
  providers: readonly ProviderInfo[];
  value: ModelChoice;
  onChange: (next: ModelChoice) => void;
  side?: "top" | "bottom";
  disabled?: boolean;
}) {
  const [open, setOpen] = useState(false);
  const info = providers.find((p) => p.id === value.provider);
  const current = resolveModel(info, value.model);
  const label = current?.name ?? info?.name ?? "Choose model";
  const effortLabel = current?.efforts.length ? (value.effort ?? null) : null;
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        disabled={disabled}
        render={
          <button type="button" className={triggerClass} aria-label={`Model: ${label}${effortLabel ? ` · ${effortLabel}` : ""}`} title={[info?.name, label, effortLabel].filter(Boolean).join(" · ")} />
        }
      >
        {info && <ProviderGlyph provider={info} />}
        <span className="max-w-48 min-w-0 truncate">{label}</span>
        {effortLabel && <span className="shrink-0 text-muted group-hover/model-trigger:text-fg group-data-popup-open/model-trigger:text-fg">{effortLabel}</span>}
        <ChevronDown className="size-3 shrink-0 text-subtle" aria-hidden />
      </PopoverTrigger>
      <PopoverPopup side={side} align="start" sideOffset={6} aria-label="Choose a model" className="overflow-hidden">
        {open && <PickerContent providers={providers} value={value} onChange={onChange} onClose={() => setOpen(false)} />}
      </PopoverPopup>
    </Popover>
  );
}

function PickerContent({ providers, value, onChange, onClose }: { providers: readonly ProviderInfo[]; value: ModelChoice; onChange: (next: ModelChoice) => void; onClose: () => void }) {
  const prefs = useModelPrefs();
  const favorites = useMemo(() => new Set(prefs.favorites), [prefs.favorites]);
  // Every model of every installed provider, in the machine's order.
  const all = useMemo<PickerModel[]>(
    () => providers.flatMap((p) => p.models.map((m) => ({ key: modelKey(p.id, m.id), provider: p.id, providerName: p.name, model: m.id, name: m.name }))),
    [providers],
  );
  const byKey = useMemo(() => new Map(all.map((m) => [m.key, m])), [all]);
  const activeInfo = providers.find((p) => p.id === value.provider);
  const active = resolveModel(activeInfo, value.model);
  const activeKey = activeInfo && active ? modelKey(activeInfo.id, active.id) : null;
  const favoritesHere = all.filter((m) => favorites.has(m.key));
  const recentHere = prefs.recent.filter((k) => byKey.has(k));
  // T3: Favorites when there are some, else the provider in use.
  const [tab, setTab] = useState<Tab>(() => (favoritesHere.length ? "favorites" : { provider: activeInfo?.id ?? providers[0]?.id ?? "" }));
  const [query, setQuery] = useState("");
  const searching = query.trim().length > 0;
  const search = useRef<HTMLInputElement>(null);
  const efforts = useRef<HTMLDivElement>(null);
  const highlighted = useRef<string | null>(null);

  const tabs: Tab[] = [
    ...(favoritesHere.length || tab === "favorites" ? ["favorites" as const] : []),
    ...(recentHere.length ? ["recent" as const] : []),
    ...providers.map((p) => ({ provider: p.id })),
  ];
  const shown: PickerModel[] = searching
    ? searchModels(all, query, favorites)
    : tab === "favorites"
      ? favoritesHere
      : tab === "recent"
        ? recentHere.map((k) => byKey.get(k)!)
        : sortModels(
            all.filter((m) => m.provider === tab.provider),
            favorites,
          );
  const shownKeys = shown.map((m) => m.key);

  const focusEfforts = () => requestAnimationFrame(() => efforts.current?.querySelector<HTMLElement>('[aria-checked="true"]')?.focus());
  const pick = (key: ModelKey) => {
    const m = byKey.get(key);
    const p = providers.find((x) => x.id === m?.provider);
    const info = p?.models.find((x) => x.id === m?.model);
    if (!m || !p || !info) return;
    modelPrefs.used(key);
    if (key !== activeKey) onChange({ provider: m.provider, model: m.model, effort: null });
    if (info.efforts.length) focusEfforts();
    else onClose();
  };
  const switchTab = (by: number) => {
    const i = tabs.findIndex((t) => sameTab(t, tab));
    const next = tabs[(i + by + tabs.length) % tabs.length];
    if (next) {
      setTab(next);
      setQuery("");
    }
  };

  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.nativeEvent.isComposing) return;
    const plain = !e.altKey && !e.ctrlKey && !e.metaKey;
    if ((e.metaKey || e.ctrlKey) && e.shiftKey && (e.key === "ArrowUp" || e.key === "ArrowDown")) {
      e.preventDefault();
      switchTab(e.key === "ArrowUp" ? -1 : 1);
      return;
    }
    if (!searching && plain && ((e.key === "ArrowLeft" && !e.shiftKey && query === "") || (e.key === "Tab" && e.shiftKey))) {
      const bar = e.currentTarget.closest("[data-model-picker]")?.querySelector("[data-model-picker-sidebar]");
      const b = bar?.querySelector<HTMLButtonElement>('button[aria-pressed="true"]') ?? bar?.querySelector<HTMLButtonElement>("button");
      if (b) {
        e.preventDefault();
        b.focus();
        return;
      }
    }
    if (e.key === "ArrowRight" && plain && !e.shiftKey && e.currentTarget.selectionStart === query.length) {
      e.preventDefault();
      // The column shows the picked model's levels, so a highlighted model is picked first.
      if (highlighted.current && highlighted.current !== activeKey) pick(highlighted.current);
      else focusEfforts();
      return;
    }
    if (e.key === "Enter" && highlighted.current) {
      (e as typeof e & { preventBaseUIHandler?: () => void }).preventBaseUIHandler?.();
      e.preventDefault();
      pick(highlighted.current);
    }
  };

  return (
    <div data-model-picker className="flex max-h-(--available-height) flex-col">
      <div className="flex h-86.5 min-h-0 max-sm:h-auto max-sm:flex-col">
        <div className="relative flex w-screen max-w-90 min-w-0 flex-row overflow-hidden max-sm:h-72 max-sm:max-w-none">
          {!searching && (
            <Sidebar
              tabs={tabs}
              tab={tab}
              providers={providers}
              onSelect={(t) => {
                setTab(t);
                search.current?.focus();
              }}
              onFocusSearch={() => search.current?.focus()}
            />
          )}
          <Combobox<string>
            inline
            open
            items={all.map((m) => m.key)}
            filteredItems={shownKeys}
            filter={null}
            autoHighlight
            value={activeKey}
            onItemHighlighted={(k) => (highlighted.current = typeof k === "string" ? k : null)}
            onValueChange={(k) => typeof k === "string" && pick(k)}
          >
            <div className={cn("flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-fg/[0.02]", !searching && "border-l border-line")}>
              <ComboboxSearchInput ref={search} autoFocus placeholder="Search models..." aria-label="Search models" value={query} onChange={(e) => setQuery(e.target.value)} onKeyDown={onSearchKey} />
              <ComboboxList aria-label="Models">
                {shown.map((m) => (
                  <ModelRow
                    key={m.key}
                    model={m}
                    provider={providers.find((p) => p.id === m.provider)!}
                    favorite={favorites.has(m.key)}
                    selected={m.key === activeKey}
                    starOnHover={tab === "favorites" && !searching}
                  />
                ))}
              </ComboboxList>
              <ComboboxEmpty>{tab === "favorites" && !searching ? "No favorites yet" : "No models found"}</ComboboxEmpty>
            </div>
          </Combobox>
        </div>
        <div
          ref={efforts}
          data-model-picker-effort
          className="flex min-h-0 w-48 flex-col border-l border-line bg-fg/[0.02] sm:shrink-0 max-sm:h-56 max-sm:w-full max-sm:border-t max-sm:border-l-0"
          onKeyDown={(e) => {
            if (e.key === "ArrowLeft" && !e.altKey && !e.ctrlKey && !e.metaKey) {
              e.preventDefault();
              search.current?.focus();
            }
          }}
        >
          <EffortColumn
            model={active}
            effort={value.effort}
            onPick={(effort) => {
              onChange({ ...value, model: active?.id ?? value.model, effort });
              onClose();
            }}
          />
        </div>
      </div>
    </div>
  );
}

function Sidebar({ tabs, tab, providers, onSelect, onFocusSearch }: { tabs: Tab[]; tab: Tab; providers: readonly ProviderInfo[]; onSelect: (t: Tab) => void; onFocusSearch: () => void }) {
  const box = useRef<HTMLDivElement>(null);
  const index = tabs.findIndex((t) => sameTab(t, tab));
  // One button is 36px with a 4px gap; the divider after Favorites/Recent adds 5px.
  const lead = tabs.filter((t) => typeof t === "string").length;
  const top = 4 + index * 40 + (index >= lead && lead > 0 ? 5 : 0) + 8;
  const move = (e: KeyboardEvent, by: number) => {
    const buttons = [...(box.current?.querySelectorAll<HTMLButtonElement>("button") ?? [])];
    const i = buttons.indexOf(e.target as HTMLButtonElement);
    buttons[(i + by + buttons.length) % buttons.length]?.focus();
  };
  const button = (t: Tab, label: string, icon: ReactNode, extra?: { hint?: string; dim?: boolean }) => (
    <button
      key={typeof t === "string" ? t : t.provider}
      type="button"
      onClick={() => onSelect(t)}
      aria-pressed={sameTab(t, tab)}
      aria-label={label}
      title={extra?.hint ? `${label} (${extra.hint})` : label}
      className={cn(
        "relative flex aspect-square w-full cursor-pointer items-center justify-center rounded-md text-muted transition-colors hover:bg-fg/10 hover:text-fg focus-visible:bg-fg/10 focus-visible:outline-none aria-pressed:text-fg",
        extra?.dim && "opacity-50",
      )}
    >
      {icon}
    </button>
  );
  return (
    <div
      ref={box}
      data-model-picker-sidebar
      role="toolbar"
      aria-orientation="vertical"
      aria-label="Providers"
      className="w-11 shrink-0 overflow-y-auto overscroll-contain bg-fg/[0.03] [scrollbar-width:none]"
      onKeyDown={(e) => {
        if (e.altKey || e.ctrlKey || e.metaKey) return;
        if (e.key === "ArrowDown" || e.key === "ArrowUp") {
          e.preventDefault();
          move(e, e.key === "ArrowDown" ? 1 : -1);
        } else if (e.key === "ArrowRight" || (e.key === "Tab" && !e.shiftKey)) {
          e.preventDefault();
          onFocusSearch();
        }
      }}
    >
      <div className="relative flex min-h-full flex-col gap-1 p-1">
        {index >= 0 && (
          <div
            aria-hidden
            className="pointer-events-none absolute right-1 z-10 h-5 w-0.75 rounded-full bg-accent transition-[top] duration-200 ease-out motion-reduce:transition-none"
            style={{ top }}
          />
        )}
        {tabs.includes("favorites") && button("favorites", "Favorites", <Star className="size-4 fill-current" />)}
        {tabs.includes("recent") && button("recent", "Recent", <Clock className="size-4" />)}
        {lead > 0 && <div className="border-b border-line" aria-hidden />}
        {providers.map((p) => button({ provider: p.id }, p.name, <ProviderGlyph provider={p} size="lg" />, p.auth === "missing" ? { hint: "log in first", dim: true } : undefined))}
      </div>
    </div>
  );
}

function ModelRow({ model, provider, favorite, selected, starOnHover }: { model: PickerModel; provider: ProviderInfo; favorite: boolean; selected: boolean; starOnHover: boolean }) {
  const label = favorite ? "Remove from favorites" : "Add to favorites";
  return (
    <ComboboxItem value={model.key} className="group relative w-full min-w-0" aria-selected={selected}>
      <ProviderGlyph provider={provider} />
      <span className="min-w-0 flex-1 truncate leading-snug">
        {model.name}
        <span className="sr-only">, {model.providerName}</span>
      </span>
      {provider.auth === "missing" && <span className="shrink-0 text-[11px] text-subtle">log in first</span>}
      <span className="flex shrink-0 items-center gap-1.5">
        <button
          type="button"
          aria-label={label}
          title={label}
          className="-mr-1 grid size-5 place-items-center rounded text-subtle outline-none hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/60"
          onPointerDown={(e) => e.stopPropagation()}
          onClick={(e) => {
            e.stopPropagation();
            e.preventDefault();
            modelPrefs.setFavorite(model.key, !favorite);
          }}
          onKeyDown={(e) => e.stopPropagation()}
        >
          <Star
            className={cn(
              "size-3",
              favorite && "fill-current text-accent",
              (!favorite || starOnHover) && "opacity-0 group-hover:opacity-100 group-data-highlighted:opacity-100 in-focus-visible:opacity-100 [@media(hover:none)]:opacity-100",
            )}
          />
        </button>
        <Check className={cn("size-3.5", !selected && "invisible")} aria-hidden />
      </span>
    </ComboboxItem>
  );
}

const COLUMN_ROW =
  "flex h-8 w-full shrink-0 cursor-pointer items-center gap-2 rounded-sm px-2 text-left text-[13px] text-fg outline-none hover:bg-fg/[0.08] focus-visible:bg-fg/[0.08] aria-checked:bg-fg/[0.06]";

const EFFORT_LABELS: Record<string, string> = { xhigh: "Extra high", xlow: "Extra low" };

/** The picked model's effort levels (T3's `ComposerEffortColumn`). */
function EffortColumn({ model, effort, onPick }: { model: ModelInfo | undefined; effort: string | null; onPick: (effort: string | null) => void }) {
  const levels: { id: string | null; label: string }[] = model?.efforts.length
    ? [{ id: null, label: "Default" }, ...model.efforts.map((e) => ({ id: e, label: EFFORT_LABELS[e] ?? e[0].toUpperCase() + e.slice(1) }))]
    : [];
  const picked = levels.some((l) => l.id === effort) ? effort : null;
  return (
    <>
      <div className="shrink-0 px-3 pt-2.5">
        <div className="flex h-8.25 -translate-y-px items-center gap-2 border-b border-line pb-1.5 text-[13px]">
          <span className="text-muted">Effort</span>
          <span className="ms-auto min-w-0 truncate text-[11.5px] text-subtle">{model?.name}</span>
        </div>
      </div>
      <div
        role="radiogroup"
        aria-label="Effort"
        className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto overscroll-contain px-2 pt-2.5 pb-1.5"
        onKeyDown={(e) => {
          if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
          e.preventDefault();
          const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[role=radio]")];
          const i = rows.indexOf(document.activeElement as HTMLElement);
          rows[(i + (e.key === "ArrowDown" ? 1 : -1) + rows.length) % rows.length]?.focus();
        }}
      >
        {levels.length === 0 ? (
          <div className="flex h-8 items-center px-2 text-[13px] text-subtle">No effort levels</div>
        ) : (
          levels.map((l) => (
            <button key={l.id ?? ""} type="button" role="radio" aria-checked={l.id === picked} tabIndex={l.id === picked ? 0 : -1} onClick={() => onPick(l.id)} className={COLUMN_ROW}>
              <span className="min-w-0 flex-1 truncate">{l.label}</span>
              <Check className={cn("size-3.5", l.id !== picked && "invisible")} aria-hidden />
            </button>
          ))
        )}
      </div>
    </>
  );
}

/** Known providers get a distinct monochrome icon (roll draws no third-party logos); others their initial. */
const PROVIDER_ICONS: Record<string, typeof Star> = { claude: Asterisk, codex: Hexagon, gemini: Sparkle, opencode: Braces, cursor: MousePointer2 };

/** A provider's mark in a small rounded square. */
export function ProviderGlyph({ provider, size = "sm" }: { provider: Pick<ProviderInfo, "id" | "name">; size?: "sm" | "lg" }) {
  const Icon = PROVIDER_ICONS[provider.id];
  return (
    <span
      aria-hidden
      className={cn(
        "grid shrink-0 place-items-center rounded-[4px] border border-line-2 bg-panel-2 font-mono leading-none font-semibold text-muted uppercase",
        size === "lg" ? "size-5 text-[10px] [&>svg]:size-3" : "size-4 text-[9px] [&>svg]:size-2.5",
      )}
    >
      {Icon ? <Icon strokeWidth={2} /> : (provider.name || provider.id).slice(0, 1)}
    </span>
  );
}

export const runtimeModes: { id: RuntimeMode; label: string; hint: string; icon: typeof ShieldOff }[] = [
  { id: "full_access", label: "Full access", hint: "Runs commands and edits files without asking", icon: ShieldOff },
  { id: "auto_edit", label: "Auto edit", hint: "Edits in the workspace freely; asks before commands", icon: PencilLine },
  { id: "ask", label: "Ask", hint: "Asks before everything", icon: ShieldCheck },
];

/** What a new thread runs with unless its project says otherwise (T3's default: full access). */
export const DEFAULT_RUNTIME_MODE: RuntimeMode = "full_access";

/** Permissions change rarely: a chip naming the mode, the choice one menu deeper (T3's `PermissionsMenu`). */
export function PermissionsMenu({ value, onChange, side = "top" }: { value: RuntimeMode; onChange: (mode: RuntimeMode) => void; side?: "top" | "bottom" }) {
  const current = runtimeModes.find((r) => r.id === value) ?? runtimeModes[0];
  const Icon = current.icon;
  return (
    <Menu>
      <MenuTrigger
        render={
          <button
            type="button"
            aria-label={`Permissions: ${current.label}`}
            title={current.hint}
            className="flex h-7 cursor-pointer items-center gap-1.5 rounded-md px-1.5 text-[12px] text-muted outline-none hover:bg-fg/[0.06] hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/60 data-popup-open:bg-fg/[0.06] data-popup-open:text-fg"
          />
        }
      >
        <Icon aria-hidden className="size-3.5" />
        {current.label}
        <ChevronDown aria-hidden className="size-3 text-subtle" />
      </MenuTrigger>
      <MenuPopup side={side} align="start" className="w-72">
        <MenuRadioGroup value={value} onValueChange={(v) => onChange(v as RuntimeMode)}>
          {runtimeModes.map((r) => (
            <MenuRadioItem key={r.id} value={r.id} className="items-start py-1.5">
              <span className="flex items-start gap-2">
                <r.icon aria-hidden className="mt-0.5 size-3.5 text-muted" />
                <span>
                  <span className="block">{r.label}</span>
                  <span className="block text-[11.5px] leading-snug text-subtle">{r.hint}</span>
                </span>
              </span>
            </MenuRadioItem>
          ))}
        </MenuRadioGroup>
      </MenuPopup>
    </Menu>
  );
}
