"use client";

/**
 * The model picker, T3 Code's (MIT) on roll: `ProviderModelPicker`, `ModelPickerContent`,
 * `ModelListRow`, `ModelPickerSidebar`, and the effort column and footer of `ComposerModelEffort`
 * (`ComposerEffortColumn`, `OptionMenuRow`, `OptionSwitchRow`, `ComposerPickerFooter`'s speed).
 *
 * The trigger shows the provider's logo, the model, its reasoning level, a bolt when it runs fast
 * and its context window (T3's `describeComposerEffort`). The popup has a sidebar (Favorites,
 * Recent, then one logo per provider, a bar marking the one shown; a provider that isn't ready says
 * why), a search across every provider, the list (favourites first, older models under "Show
 * more", a star and a check on each row, ⌘1–⌘9 on the first nine), what the shown provider needs
 * (installing, logging in, an update for newer models), the picked model's options in a column on
 * the right (its reasoning levels, then one row per other option: a menu for a choice, a switch
 * for on/off), and its speed as a segmented control underneath. Every option is the daemon's
 * description of it (`ModelInfo.options`), so a new provider or option needs nothing here.
 *
 * Keyboard as T3: type to search, arrows and Enter in the list, ⌘/Ctrl+1–9 to pick one of the
 * first nine, ← (empty search) or Shift+Tab to the providers, → to the options,
 * ⌘/Ctrl+Shift+↑/↓ to switch providers, Escape to close. Picking a model with reasoning levels
 * moves to them; picking a level (or a model without options) closes it. With `shortcuts` (the
 * composer's picker), ⌘/Ctrl+Shift+M opens it and ⌘/Ctrl+Shift+E steps through the levels
 * (T3's `modelPicker.toggle` and `composer.effort`). With `multi` (a new thread's composer),
 * Shift-click or Shift+Enter adds a model to the ones a prompt starts, one thread each (T3's
 * multi-model fan-out); the trigger then names the first two and how many more.
 */

import { Check, ChevronDown, ChevronRight, Clock, PencilLine, ShieldCheck, ShieldOff, Star, Zap } from "lucide-react";
import { useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import type { ModelInfo, ModelOption, ModelOptions, OptionValue, ProviderInfo, RuntimeMode } from "@/lib/agents-api";
import { modelKey, modelPrefs, searchModels, sortModels, useModelPrefs, type ModelKey, type PickerModel } from "@/lib/agents-models";
import { acceptedOptions, effortValue, isFast, nextEffort, optionValue, otherOptions, reasoningOf, speedOf, traits, type BooleanOption, type SelectOption } from "@/lib/agents-model-options";
import { ProviderLogo } from "./provider-logo";
import { exhausted, usageSummary } from "@/lib/agents-usage";
import { loginCommand } from "./shared";
import {
  cn,
  Combobox,
  ComboboxEmpty,
  ComboboxItem,
  ComboboxList,
  ComboboxSearchInput,
  Menu,
  MenuPopup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuTrigger,
  Popover,
  PopoverPopup,
  PopoverTrigger,
  Segmented,
  Switch,
} from "./ui";

/** The model a provider runs when none (or an unknown one) is picked: its default, else its first. */
export function resolveModel(info: ProviderInfo | undefined, model: string | null | undefined): ModelInfo | undefined {
  const models = info?.models ?? [];
  return models.find((m) => m.id === model) ?? models.find((m) => m.default) ?? models[0];
}

/**
 * Several models at once (T3's `draftFanoutSelections`): `selected` is null while one model is
 * picked; `onToggle` adds or removes one (Shift-click, Shift+Enter).
 */
export interface MultiPick {
  selected: readonly ModelChoice[] | null;
  onToggle: (choice: ModelChoice) => void;
  /** A plain pick while several are picked: that one alone again (T3). */
  onSingle: (choice: ModelChoice) => void;
}

/** The same provider and model (options and effort aside). */
export const sameModel = (a: Pick<ModelChoice, "provider" | "model">, b: Pick<ModelChoice, "provider" | "model">) => a.provider === b.provider && a.model === b.model;

/** T3's multi-model trigger label: the first two names, then how many more ("Choose models" for none). */
export function modelsLabel(choices: readonly ModelChoice[], providers: readonly ProviderInfo[]): string {
  const names = choices.map((c) => {
    const m = resolveModel(providers.find((p) => p.id === c.provider), c.model);
    return m?.short_name ?? m?.name ?? c.model ?? c.provider;
  });
  if (names.length === 0) return "Choose models";
  return names.length <= 2 ? names.join(", ") : `${names.slice(0, 2).join(", ")}, ${names.length - 2} more`;
}

export interface ModelChoice {
  provider: string;
  model: string | null;
  /** The reasoning level ([`reasoningOf`]); `null` runs the model's default. */
  effort: string | null;
  /** The model's other options by id; a missing one runs its default. */
  options: ModelOptions;
}

/** A choice with nothing picked but the provider. */
export const choiceOf = (provider: string, model: string | null = null): ModelChoice => ({ provider, model, effort: null, options: {} });

/** Why a provider's models can't be picked, if they can't: it isn't installed (or is still being checked). */
export function unavailable(p: ProviderInfo): string | null {
  if (p.installed) return null;
  return p.checking ? "Checking…" : "Not installed";
}

/** What a provider needs before it works, for the picker's notices (T3's `getProviderStatusMessage`). */
export function providerNotice(p: ProviderInfo): { text: string; command?: string; checking?: boolean } | null {
  if (p.checking && !p.installed) return { text: `Checking ${p.name}…`, checking: true };
  if (!p.installed) return { text: p.detail ?? `${p.name} isn't installed on this machine.` };
  if (p.auth === "missing") return { text: `${p.name} isn't logged in. Log in from a terminal on the machine:`, command: loginCommand(p) };
  // An ACP agent names its models only in a session: the first one it opens tells the machine.
  if (p.models.length === 0 && p.id.startsWith("acp:")) return { text: `${p.name} lists its models once a session starts: start a thread with its default model, and they show here after.` };
  if (p.models.length === 0) return { text: p.detail ?? `${p.name} reported no models.` };
  return null;
}

/** "Update Claude Code to v2.1.293 or newer to use Claude Haiku 5.5." (T3's `formatProviderUpdateRequiredNotice`). */
export function updateNotice(p: ProviderInfo, query = ""): string | null {
  const q = query.trim().toLowerCase();
  const models = p.update_required.filter((m) => !q || m.name.toLowerCase().includes(q) || m.id.toLowerCase().includes(q));
  if (models.length === 0) return null;
  const newest = models.map((m) => m.min_version).reduce((a, b) => (compareVersions(b, a) > 0 ? b : a));
  const names = models.map((m) => m.name);
  const list = names.length <= 2 ? names.join(" and ") : `${names.slice(0, -1).join(", ")}, and ${names[names.length - 1]}`;
  return `Update ${p.name} to v${newest.replace(/^v/, "")} or newer to use ${list}.`;
}

function compareVersions(a: string, b: string) {
  const pa = a.replace(/^v/, "").split(".").map((x) => parseInt(x, 10) || 0);
  const pb = b.replace(/^v/, "").split(".").map((x) => parseInt(x, 10) || 0);
  for (let i = 0; i < Math.max(pa.length, pb.length); i++) if ((pa[i] ?? 0) !== (pb[i] ?? 0)) return (pa[i] ?? 0) - (pb[i] ?? 0);
  return 0;
}

type Tab = "favorites" | "recent" | { provider: string };
const sameTab = (a: Tab, b: Tab) => (typeof a === "string" || typeof b === "string" ? a === b : a.provider === b.provider);
/** The "Show more" row of a provider's older models, an item of the list so the keyboard reaches it. */
const legacyKey = (provider: string) => `legacy:${provider}`;

const isMac = () => typeof navigator !== "undefined" && /Mac|iPhone|iPad/.test(navigator.platform);
const jumpLabel = (i: number) => (isMac() ? `⌘${i + 1}` : `Ctrl ${i + 1}`);

const triggerClass =
  "group/model-trigger inline-flex h-7 min-w-0 cursor-pointer items-center gap-1.5 rounded-md px-1.5 text-[12px] text-fg outline-none transition-colors hover:bg-fg/[0.06] focus-visible:ring-2 focus-visible:ring-accent/60 data-popup-open:bg-fg/[0.06] disabled:cursor-not-allowed disabled:opacity-50";

/**
 * The provider, model, effort and options of a thread, from the machine's providers. The owner
 * keeps the choice: `onChange` gets the whole of it (a new model keeps the effort and options it
 * takes).
 */
export function ModelPicker({
  providers,
  value,
  onChange,
  side = "top",
  disabled = false,
  shortcuts = false,
  multi,
}: {
  providers: readonly ProviderInfo[];
  value: ModelChoice;
  onChange: (next: ModelChoice) => void;
  side?: "top" | "bottom";
  disabled?: boolean;
  /** ⌘/Ctrl+Shift+M opens it, ⌘/Ctrl+Shift+E steps the reasoning level: one picker on the page (the composer's). */
  shortcuts?: boolean;
  /** Several models at once (a new thread's composer). */
  multi?: MultiPick;
}) {
  const [open, setOpen] = useState(false);
  const info = providers.find((p) => p.id === value.provider);
  const current = resolveModel(info, value.model);
  const many = multi?.selected && multi.selected.length > 1 ? multi.selected : null;
  const label = many ? modelsLabel(many, providers) : (current?.short_name ?? current?.name ?? info?.name ?? "Choose model");
  const t = traits(current, value.effort, value.options);
  const latest = useRef({ value, current, onChange, disabled });
  latest.current = { value, current, onChange, disabled };

  useEffect(() => {
    if (!shortcuts) return;
    const onKey = (e: globalThis.KeyboardEvent) => {
      if (!(e.metaKey || e.ctrlKey) || !e.shiftKey || e.altKey || latest.current.disabled) return;
      const key = e.key.toLowerCase();
      if (key === "m") {
        e.preventDefault();
        setOpen((o) => !o);
      } else if (key === "e") {
        const { value: v, current: m, onChange: change } = latest.current;
        const next = nextEffort(m, v.effort);
        if (!next || !m) return;
        e.preventDefault();
        change({ ...v, model: m.id, effort: next });
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [shortcuts]);

  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        disabled={disabled}
        render={
          <button
            type="button"
            className={triggerClass}
            aria-label={many ? `Models: ${label}` : `Model: ${label}${t.detail ? ` · ${t.detail}` : ""}`}
            title={
              many
                ? `One thread each: ${modelsLabel(many.slice(0, 1), providers)}${many.slice(1).map((c) => `, ${modelsLabel([c], providers)}`).join("")}`
                : [info?.name, current?.name ?? label, t.detail, shortcuts ? `${isMac() ? "⌘⇧M" : "Ctrl+Shift+M"}` : null].filter(Boolean).join(" · ")
            }
          />
        }
      >
        {many ? (
          <span className="flex shrink-0 -space-x-1" aria-hidden>
            {[...new Set(many.map((c) => c.provider))].slice(0, 3).map((id) => {
              const p = providers.find((x) => x.id === id);
              return p ? <ProviderLogo key={id} provider={p} className="size-3.5 rounded-full bg-panel" /> : null;
            })}
          </span>
        ) : (
          info && <ProviderLogo provider={info} className="size-3.5" />
        )}
        <span className="max-w-48 min-w-0 truncate">{label}</span>
        {!many && (t.effort || t.fast || t.context) && (
          <span className="flex shrink-0 items-center gap-1 text-muted group-hover/model-trigger:text-fg group-data-popup-open/model-trigger:text-fg">
            {t.effort && <span className="max-w-24 truncate">{t.effort}</span>}
            {t.fast && <Zap aria-hidden className="size-3 fill-current" />}
            {t.context && <span className="max-w-16 truncate text-[11px]">{t.context}</span>}
          </span>
        )}
        <ChevronDown className="size-3 shrink-0 text-subtle" aria-hidden />
      </PopoverTrigger>
      <PopoverPopup side={side} align="start" sideOffset={6} aria-label="Choose a model" className="overflow-hidden">
        {open && <PickerContent providers={providers} value={value} onChange={onChange} onClose={() => setOpen(false)} multi={multi} />}
      </PopoverPopup>
    </Popover>
  );
}

function PickerContent({
  providers,
  value,
  onChange,
  onClose,
  multi,
}: {
  providers: readonly ProviderInfo[];
  value: ModelChoice;
  onChange: (next: ModelChoice) => void;
  onClose: () => void;
  multi?: MultiPick;
}) {
  // Shift held on the click or Enter that picks: add the model instead (T3's additive pick).
  const additive = useRef(false);
  // A click on the active row that the list already handled (Base UI reports it too).
  const handled = useRef(false);
  const prefs = useModelPrefs();
  const favorites = useMemo(() => new Set(prefs.favorites), [prefs.favorites]);
  // Every model of every provider on offer, in the machine's order.
  const all = useMemo<(PickerModel & { info: ModelInfo; legacy: boolean })[]>(
    () => providers.flatMap((p) => p.models.map((m) => ({ key: modelKey(p.id, m.id), provider: p.id, providerName: p.name, model: m.id, name: m.name, info: m, legacy: !!m.legacy }))),
    [providers],
  );
  const byKey = useMemo(() => new Map(all.map((m) => [m.key, m])), [all]);
  const providerOf = (id: string) => providers.find((p) => p.id === id);
  const activeInfo = providerOf(value.provider);
  const active = resolveModel(activeInfo, value.model);
  const activeKey = activeInfo && active ? modelKey(activeInfo.id, active.id) : null;
  const favoritesHere = all.filter((m) => favorites.has(m.key));
  const recentHere = prefs.recent.filter((k) => byKey.has(k));
  // T3: Favorites when there are some, else the provider in use.
  const [tab, setTab] = useState<Tab>(() => (favoritesHere.length ? "favorites" : { provider: activeInfo?.id ?? providers[0]?.id ?? "" }));
  const [query, setQuery] = useState("");
  // Older models are folded until asked for, unless the one in use is one of them.
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set(active?.legacy && activeInfo ? [activeInfo.id] : []));
  const searching = query.trim().length > 0;
  const search = useRef<HTMLInputElement>(null);
  const column = useRef<HTMLDivElement>(null);
  const highlighted = useRef<string | null>(null);

  const tabs: Tab[] = [
    ...(favoritesHere.length || tab === "favorites" ? ["favorites" as const] : []),
    ...(recentHere.length ? ["recent" as const] : []),
    ...providers.map((p) => ({ provider: p.id })),
  ];
  const tabProvider = typeof tab === "string" ? undefined : providerOf(tab.provider);
  let legacyRow: { provider: string; count: number; open: boolean } | null = null;
  let shown: PickerModel[];
  if (searching) shown = searchModels(all, query, favorites);
  else if (tab === "favorites") shown = favoritesHere;
  else if (tab === "recent") shown = recentHere.map((k) => byKey.get(k)!);
  else {
    const here = all.filter((m) => m.provider === tab.provider);
    const older = here.filter((m) => m.legacy);
    const open = expanded.has(tab.provider);
    shown = sortModels(open ? here : here.filter((m) => !m.legacy), favorites);
    if (older.length) legacyRow = { provider: tab.provider, count: older.length, open };
  }
  const pickable = (m: PickerModel) => !unavailable(providerOf(m.provider)!);
  const jumps = shown.filter(pickable).slice(0, 9).map((m) => m.key);
  const shownKeys = [...shown.map((m) => m.key), ...(legacyRow ? [legacyKey(legacyRow.provider)] : [])];

  const focusColumn = () =>
    requestAnimationFrame(() => (column.current?.querySelector<HTMLElement>('[role="radio"][aria-checked="true"]') ?? column.current?.querySelector<HTMLElement>("[data-option-row]"))?.focus());
  const pick = (key: ModelKey, add = false) => {
    if (key.startsWith("legacy:")) {
      const p = key.slice("legacy:".length);
      setExpanded((s) => {
        const next = new Set(s);
        if (next.has(p)) next.delete(p);
        else next.add(p);
        return next;
      });
      return;
    }
    const m = byKey.get(key);
    const p = m && providerOf(m.provider);
    if (!m || !p || unavailable(p)) return;
    modelPrefs.used(key);
    const same = p.id === value.provider && m.model === active?.id;
    const choice: ModelChoice = same ? { ...value, model: m.model } : { provider: p.id, model: m.model, effort: null, options: {} };
    if (add && multi) return multi.onToggle(choice);
    if (multi?.selected && multi.selected.length > 1) {
      multi.onSingle(choice);
      if (m.info.options.length) focusColumn();
      else onClose();
      return;
    }
    if (key !== activeKey) {
      const sameProvider = p.id === value.provider;
      onChange({
        provider: p.id,
        model: m.model,
        // What still applies to the new model stays (agent-proto `ModelInfo::accepted`).
        effort: sameProvider && value.effort && reasoningOf(m.info)?.choices.some((c) => c.id === value.effort) ? value.effort : null,
        options: sameProvider ? acceptedOptions(m.info, value.options) : {},
      });
    }
    if (m.info.options.length) focusColumn();
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
    // ⌘1–9: one of the first nine (T3's `modelPicker.jump.N`).
    if ((e.metaKey || e.ctrlKey) && !e.shiftKey && !e.altKey && /^[1-9]$/.test(e.key)) {
      const key = jumps[Number(e.key) - 1];
      if (key) {
        e.preventDefault();
        pick(key);
      }
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
      // The column shows the picked model's options, so a highlighted model is picked first.
      if (highlighted.current && highlighted.current !== activeKey) pick(highlighted.current);
      else focusColumn();
      return;
    }
    if (e.key === "Enter" && highlighted.current) {
      (e as typeof e & { preventBaseUIHandler?: () => void }).preventBaseUIHandler?.();
      e.preventDefault();
      pick(highlighted.current, e.shiftKey);
    }
  };

  const setEffort = (effort: string) => {
    onChange({ ...value, model: active?.id ?? value.model, effort });
    onClose();
  };
  const setOption = (id: string, v: OptionValue) => onChange({ ...value, model: active?.id ?? value.model, options: { ...value.options, [id]: v } });
  const notice = tabProvider ? providerNotice(tabProvider) : null;
  const updates = (searching ? providers : tabProvider ? [tabProvider] : []).map((p) => updateNotice(p, searching ? query : "")).filter((n): n is string => !!n);
  const speed = speedOf(active);

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
            items={[...all.map((m) => m.key), ...providers.map((p) => legacyKey(p.id))]}
            filteredItems={shownKeys}
            filter={null}
            autoHighlight
            value={activeKey}
            onItemHighlighted={(k) => (highlighted.current = typeof k === "string" ? k : null)}
            onValueChange={(k) => {
              const add = additive.current;
              additive.current = false;
              if (handled.current) {
                handled.current = false;
                return;
              }
              if (typeof k === "string") pick(k, add);
            }}
          >
            <div className={cn("flex min-h-0 min-w-0 flex-1 flex-col overflow-hidden bg-fg/[0.02]", !searching && "border-l border-line")}>
              <ComboboxSearchInput ref={search} autoFocus placeholder="Search models..." aria-label="Search models" value={query} onChange={(e) => setQuery(e.target.value)} onKeyDown={onSearchKey} />
              <ComboboxList
                aria-label="Models"
                onPointerDownCapture={(e) => {
                  additive.current = e.shiftKey;
                  handled.current = false;
                }}
                onClickCapture={(e) => {
                  // Base UI doesn't pick the model already picked: with several picked, a click on
                  // it still toggles it (Shift) or makes it the one model.
                  const row = (e.target as HTMLElement).closest<HTMLElement>("[data-model-key]");
                  if (multi?.selected && multi.selected.length > 1 && row?.dataset.modelKey && row.dataset.modelKey === activeKey) {
                    e.preventDefault();
                    additive.current = false;
                    handled.current = true;
                    pick(row.dataset.modelKey, e.shiftKey);
                  }
                }}
              >
                {shown.map((m) => {
                  const p = providerOf(m.provider)!;
                  const jump = jumps.indexOf(m.key);
                  return (
                    <ModelRow
                      key={m.key}
                      model={m}
                      info={byKey.get(m.key)!.info}
                      provider={p}
                      favorite={favorites.has(m.key)}
                      selected={multi?.selected && multi.selected.length > 1 ? multi.selected.some((c) => modelKey(c.provider, c.model ?? "") === m.key) : m.key === activeKey}
                      starOnHover={tab === "favorites" && !searching}
                      jump={jump >= 0 ? jumpLabel(jump) : null}
                    />
                  );
                })}
                {legacyRow && (
                  <ComboboxItem value={legacyKey(legacyRow.provider)} className="group w-full text-muted data-highlighted:text-fg">
                    <ChevronRight aria-hidden className={cn("size-3.5 transition-transform duration-150 motion-reduce:transition-none", legacyRow.open && "rotate-90")} />
                    <span>{legacyRow.open ? "Show less" : `Show more (${legacyRow.count})`}</span>
                  </ComboboxItem>
                )}
              </ComboboxList>
              <ComboboxEmpty>{tab === "favorites" && !searching ? "No favorites yet" : notice ? null : "No models found"}</ComboboxEmpty>
              {(notice || updates.length > 0) && (
                <div className="max-h-36 shrink-0 space-y-1.5 overflow-y-auto border-t border-line px-3 py-2 text-[11.5px] leading-snug text-muted" data-model-picker-notices>
                  {notice && (
                    <p className={cn(notice.checking && "shimmer")}>
                      {notice.text}
                      {notice.command && <code className="ms-1 rounded bg-fg/[0.06] px-1 font-mono text-[11px] text-fg">{notice.command}</code>}
                    </p>
                  )}
                  {updates.map((u) => (
                    <p key={u}>{u}</p>
                  ))}
                </div>
              )}
            </div>
          </Combobox>
        </div>
        <div
          ref={column}
          data-model-picker-effort
          className="flex min-h-0 w-48 flex-col border-l border-line bg-fg/[0.02] sm:shrink-0 max-sm:h-56 max-sm:w-full max-sm:border-t max-sm:border-l-0"
          onKeyDown={(e) => {
            if (e.key === "ArrowLeft" && !e.altKey && !e.ctrlKey && !e.metaKey && !(e.target as HTMLElement).closest("[data-option-menu]")) {
              e.preventDefault();
              search.current?.focus();
            }
          }}
        >
          <OptionsColumn model={active} value={value} onEffort={setEffort} onOption={setOption} />
        </div>
      </div>
      {speed && active && (
        <div className="flex items-center gap-2 border-t border-line bg-fg/[0.03] px-3 py-2" data-model-picker-footer>
          <SpeedControl option={speed} value={optionValue(speed, value.options)} onChange={(v) => setOption(speed.id, v)} />
        </div>
      )}
      {multi && (
        <p className="border-t border-line px-3 py-1.5 text-[11px] text-subtle max-sm:hidden [@media(hover:none)]:hidden" data-model-picker-multi>
          {multi.selected && multi.selected.length > 1 ? `${multi.selected.length} models: one thread each, in its own worktree. ` : ""}
          <kbd className="font-sans">⇧</kbd>-click or <kbd className="font-sans">⇧ Enter</kbd> to add a model
        </p>
      )}
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
      aria-label={extra?.hint ? `${label}: ${extra.hint}` : label}
      title={extra?.hint ? `${label}: ${extra.hint}` : label}
      className={cn(
        "relative flex aspect-square w-full cursor-pointer items-center justify-center rounded-md text-muted transition-colors hover:bg-fg/10 hover:text-fg focus-visible:bg-fg/10 focus-visible:outline-none aria-pressed:text-fg",
        extra?.dim && "opacity-50",
      )}
    >
      {icon}
    </button>
  );
  // T3's sidebar tooltip: "Name: Unavailable. <why>".
  const status = (p: ProviderInfo): { hint?: string; dim?: boolean } | undefined => {
    const why = unavailable(p);
    if (why) return { hint: p.checking ? why : `${why}.${p.detail ? ` ${p.detail}` : ""}`, dim: true };
    if (p.auth === "missing") return { hint: `Not logged in.${p.detail ? ` ${p.detail}` : ""}`, dim: true };
    // The account's usage, as its CLI reports it ("42% left · weekly").
    const usage = usageSummary(p, Date.now());
    if (usage) return { hint: usage, dim: !!exhausted(p.usage_limits) };
    return undefined;
  };
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
        {providers.map((p) => button({ provider: p.id }, p.name, <ProviderLogo provider={p} className="size-4.5" />, status(p)))}
      </div>
    </div>
  );
}

function ModelRow({
  model,
  info,
  provider,
  favorite,
  selected,
  starOnHover,
  jump,
}: {
  model: PickerModel;
  info: ModelInfo;
  provider: ProviderInfo;
  favorite: boolean;
  selected: boolean;
  starOnHover: boolean;
  jump: string | null;
}) {
  const label = favorite ? "Remove from favorites" : "Add to favorites";
  const why = unavailable(provider);
  return (
    <ComboboxItem
      value={model.key}
      disabled={!!why}
      data-model-key={model.key}
      className="group relative w-full min-w-0 data-disabled:cursor-not-allowed data-disabled:opacity-60"
      aria-selected={selected}
      title={why ? `${provider.name}: ${why}` : undefined}
    >
      <ProviderLogo provider={provider} className="size-4" />
      <span className="flex min-w-0 flex-1 items-center gap-1.5">
        <span className="min-w-0 truncate leading-snug" title={model.name}>
          {model.name}
          <span className="sr-only">, {model.providerName}</span>
        </span>
        {info.custom && <span className="shrink-0 rounded border border-line-2 px-1 text-[10px] leading-4 text-subtle">Custom</span>}
        {info.via && (
          <span className="max-w-32 shrink-0 truncate rounded border border-line-2 px-1 text-[10px] leading-4 text-subtle" title={`Listed by the endpoint ${info.via}`}>
            via {info.via}
          </span>
        )}
        {info.images === false && <span className="shrink-0 text-[10.5px] text-subtle">Text only</span>}
      </span>
      {why ? (
        <span className="shrink-0 text-[11px] text-subtle">{why}</span>
      ) : (
        (provider.auth === "missing" && <span className="shrink-0 text-[11px] text-subtle">Log in first</span>) ||
        (exhausted(provider.usage_limits) && (
          <span className="shrink-0 text-[11px] text-subtle" title={usageSummary(provider, Date.now()) ?? undefined}>
            Limit reached
          </span>
        ))
      )}
      <span className="flex shrink-0 items-center gap-1.5">
        {jump && <kbd className="font-mono text-[10px] text-subtle max-sm:hidden [@media(hover:none)]:hidden">{jump}</kbd>}
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
  "flex h-8 w-full shrink-0 cursor-pointer items-center gap-2 rounded-sm px-2 text-left text-[13px] text-fg outline-none hover:bg-fg/[0.08] focus-visible:bg-fg/[0.08] aria-checked:bg-fg/[0.06] data-popup-open:bg-fg/[0.08]";

/** Up and Down move between the column's rows (T3's `moveBetweenRows`). */
function moveBetweenRows(e: KeyboardEvent<HTMLElement>) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  if ((e.target as HTMLElement).closest("[data-option-menu]")) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[role=radio], [data-option-row]")];
  const i = rows.indexOf(document.activeElement as HTMLElement);
  if (i < 0) return;
  e.preventDefault();
  rows[(i + (e.key === "ArrowDown" ? 1 : -1) + rows.length) % rows.length]?.focus();
}

/** The picked model's options (T3's `ComposerEffortColumn`): its reasoning levels, then a row for each other option. */
function OptionsColumn({ model, value, onEffort, onOption }: { model: ModelInfo | undefined; value: ModelChoice; onEffort: (effort: string) => void; onOption: (id: string, v: OptionValue) => void }) {
  const reasoning = reasoningOf(model);
  const others = otherOptions(model);
  const picked = effortValue(model, value.effort);
  return (
    <>
      {/* Matches the search row's box, so one rule runs across the top of the popup. */}
      <div className="shrink-0 px-3 pt-2.5">
        <div className="flex h-8.25 -translate-y-px items-center gap-2 border-b border-line pb-1.5 text-[13px]">
          <span className="text-muted">{reasoning?.label ?? (others.length ? "Options" : "Effort")}</span>
          <span className="ms-auto min-w-0 truncate text-[11.5px] text-subtle">{model?.short_name ?? model?.name}</span>
        </div>
      </div>
      <div className="flex min-h-0 flex-1 flex-col gap-0.5 overflow-y-auto overscroll-contain px-2 pt-2.5 pb-1.5" onKeyDownCapture={moveBetweenRows}>
        {!reasoning && others.length === 0 && <div className="flex h-8 items-center px-2 text-[13px] text-subtle">No effort levels</div>}
        {reasoning && (
          <div role="radiogroup" aria-label={reasoning.label} className="flex flex-col gap-0.5">
            {reasoning.choices.map((c) => (
              <button
                key={c.id}
                type="button"
                role="radio"
                aria-checked={c.id === picked}
                tabIndex={c.id === picked || (!picked && c === reasoning.choices[0]) ? 0 : -1}
                title={c.description}
                onClick={() => onEffort(c.id)}
                className={COLUMN_ROW}
              >
                <span className="min-w-0 flex-1 truncate">{c.label}</span>
                {c.id === reasoning.default && <span className="shrink-0 text-[11px] text-subtle">Default</span>}
                <Check className={cn("size-3.5 shrink-0", c.id !== picked && "invisible")} aria-hidden />
              </button>
            ))}
          </div>
        )}
        {others.length > 0 && (
          <div className={cn("flex flex-col gap-0.5", reasoning && "mt-1 border-t border-line pt-1")}>
            {others.map((o) =>
              o.type === "boolean" ? (
                <OptionSwitchRow key={o.id} option={o} value={optionValue(o, value.options) === true} onChange={(v) => onOption(o.id, v)} />
              ) : (
                <OptionMenuRow key={o.id} option={o} value={optionValue(o, value.options) as string | undefined} onChange={(v) => onOption(o.id, v)} />
              ),
            )}
          </div>
        )}
      </div>
    </>
  );
}

function RowText({ label, value }: { label: string; value: string }) {
  return (
    <>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      <span className="max-w-20 shrink-0 truncate text-[12px] text-muted">{value}</span>
    </>
  );
}

/** A choice of a few opens beside the column (T3's `OptionMenuRow`). */
function OptionMenuRow({ option, value, onChange }: { option: SelectOption; value: string | undefined; onChange: (v: string) => void }) {
  const current = option.choices.find((c) => c.id === value)?.label ?? "";
  return (
    <Menu modal={false}>
      <MenuTrigger render={<button type="button" data-option-row aria-label={`${option.label}: ${current}`} title={option.description} className={COLUMN_ROW} />}>
        <RowText label={option.label} value={current} />
        <ChevronRight aria-hidden className="-me-1 size-3.5 shrink-0 text-subtle" />
      </MenuTrigger>
      <MenuPopup side="right" align="start" sideOffset={6} className="max-w-72" data-option-menu>
        <MenuRadioGroup aria-label={option.label} value={value ?? ""} onValueChange={(v) => typeof v === "string" && v !== value && onChange(v)}>
          {option.choices.map((c) => (
            <MenuRadioItem key={c.id} value={c.id} closeOnClick>
              <span className="flex min-w-0 items-center gap-3">
                <span className="flex min-w-0 flex-1 flex-col">
                  <span className="truncate">{c.label}</span>
                  {c.description && <span className="line-clamp-2 text-[11.5px] text-subtle">{c.description}</span>}
                </span>
                {c.id === option.default && <span className="shrink-0 text-[11px] text-subtle">Default</span>}
              </span>
            </MenuRadioItem>
          ))}
        </MenuRadioGroup>
      </MenuPopup>
    </Menu>
  );
}

/** On or off toggles in place (T3's `OptionSwitchRow`). */
function OptionSwitchRow({ option, value, onChange }: { option: BooleanOption; value: boolean; onChange: (v: boolean) => void }) {
  return (
    <label className={cn(COLUMN_ROW, "has-focus-visible:bg-fg/[0.08]")} title={option.description}>
      <RowText label={option.label} value={value ? "On" : "Off"} />
      <Switch size="sm" checked={value} onCheckedChange={onChange} aria-label={option.label} data-option-row />
    </label>
  );
}

/** The speed, one click away (T3's `SpeedControl`): Standard | Fast, or a provider's tiers. */
function SpeedControl({ option, value, onChange }: { option: ModelOption; value: OptionValue | undefined; onChange: (v: OptionValue) => void }) {
  const choices: { value: string; label: string; title?: string; fast: boolean }[] =
    option.type === "select"
      ? option.choices.map((c) => ({ value: c.id, label: c.label, title: c.description, fast: isFast(option, c.id) }))
      : [
          { value: "off", label: "Standard", fast: false },
          { value: "on", label: "Fast", title: option.description, fast: true },
        ];
  const current = option.type === "select" ? String(value ?? option.choices[0]?.id ?? "") : value === true ? "on" : "off";
  return (
    <Segmented
      label={option.label}
      value={current}
      onChange={(v) => onChange(option.type === "select" ? v : v === "on")}
      options={choices.map((c) => ({
        value: c.value,
        title: c.title,
        label: (
          <>
            {c.fast && <Zap aria-hidden className="fill-current" />}
            {c.label}
          </>
        ),
      }))}
    />
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
