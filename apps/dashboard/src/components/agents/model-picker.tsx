/**
 * The model picker, T3 Code's (MIT) on roll: `ProviderModelPicker`, `ModelPickerContent`,
 * `ModelListRow`, `ModelPickerSidebar`, and the effort column and footer of `ComposerModelEffort`
 * (`ComposerEffortColumn`, `OptionMenuRow`, `OptionSwitchRow`, `ComposerPickerFooter`'s speed).
 *
 * The trigger shows the provider's logo, the model, its reasoning level, a bolt when it runs fast
 * and its context window (T3's `describeComposerEffort`). The popup is 360px wide and as tall as
 * its tallest tab (T3's), up to a cap, then the list scrolls. A search runs across every provider
 * on top; under it a labelled sidebar (Favorites, Recent, then each provider by logo and name, the
 * shown one a quiet fill; one that isn't ready says why), with no sidebar for a lone provider; the
 * list (favourites first, older models folded, a star and a check on each row, ⌘1–⌘9 on the first
 * nine, the provider named where providers mix); what the shown provider needs (installing, logging
 * in, an update for newer models); and, only for a picked model that has options, one compact row
 * per option under it (a segmented control for a few short choices, a menu for more, a switch for
 * on/off). The user rejected T3's docked effort column (wave 9): it doubled the popup for two
 * levels. Every option is the daemon's description of it (`ModelInfo.options`), so a new provider or
 * option needs nothing here.
 *
 * Keyboard as T3: type to search, arrows and Enter in the list, ⌘/Ctrl+1–9 to pick one of the
 * first nine, ← (empty search) or Shift+Tab to the providers, Tab to the options (↑ from the first
 * goes back), ⌘/Ctrl+Shift+↑/↓ to switch providers, Escape to close. Picking a model with options
 * moves to them; picking a level (or a model without options) closes it. With `shortcuts` (the
 * composer's picker), ⌘/Ctrl+Shift+M opens it and ⌘/Ctrl+Shift+E steps through the levels
 * (T3's `modelPicker.toggle` and `composer.effort`). With `multi` (a new thread's composer),
 * Shift-click or Shift+Enter adds a model to the ones a prompt starts, one thread each (T3's
 * multi-model fan-out); the trigger then names the first two and how many more.
 */

import { Check, ChevronDown, ChevronRight, Clock, PencilLine, ShieldCheck, ShieldOff, Star, Zap } from "lucide-react";
import { Fragment, useEffect, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
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

/** A list row and a sidebar row are 28px; the list has 4px of padding above and below. */
const ROW = 28;
/** The most the list grows to before it scrolls (T3's 346px popup, less its search row). */
const LIST_MAX = 304;

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
  finalFocus,
  unset = "Choose model",
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
  /** Where focus goes when it closes (Base UI's; the trigger by default). */
  finalFocus?: FinalFocus;
  /** What it says with nothing chosen ("Machine's" for a default left to the next layer). */
  unset?: string;
}) {
  const [open, setOpen] = useState(false);
  const info = providers.find((p) => p.id === value.provider);
  const current = resolveModel(info, value.model);
  const many = multi?.selected && multi.selected.length > 1 ? multi.selected : null;
  const label = many ? modelsLabel(many, providers) : (current?.short_name ?? current?.name ?? info?.name ?? unset);
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
      <PopoverPopup side={side} align="start" sideOffset={6} aria-label="Choose a model" className="overflow-hidden" finalFocus={finalFocus}>
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
  const footer = useRef<HTMLDivElement>(null);
  const highlighted = useRef<string | null>(null);

  const tabs: Tab[] = [
    ...(favoritesHere.length || tab === "favorites" ? ["favorites" as const] : []),
    ...(recentHere.length ? ["recent" as const] : []),
    ...providers.map((p) => ({ provider: p.id })),
  ];
  // One provider and nothing kept: the list alone.
  const sidebar = tabs.length > 1;
  const tabProvider = typeof tab === "string" ? undefined : providerOf(tab.provider);
  const rowsOf = (t: Tab): PickerModel[] => {
    if (t === "favorites") return favoritesHere;
    if (t === "recent") return recentHere.map((k) => byKey.get(k)!);
    const here = all.filter((m) => m.provider === t.provider);
    const sorted = sortModels(expanded.has(t.provider) ? here : here.filter((m) => !m.legacy), favorites);
    // The provider's own models first, then each endpoint's together (under its label).
    const vias = [...new Set(sorted.map((m) => byKey.get(m.key)!.info.via ?? ""))].sort((a, b) => (a === "" ? -1 : b === "" ? 1 : 0));
    return vias.flatMap((v) => sorted.filter((m) => (byKey.get(m.key)!.info.via ?? "") === v));
  };
  const olderOf = (t: Tab) => (typeof t === "string" ? 0 : all.filter((m) => m.provider === t.provider && m.legacy).length);
  let legacyRow: { provider: string; count: number; open: boolean } | null = null;
  let shown: PickerModel[];
  // A provider's own models before an endpoint's copies of the same names (ties keep this order).
  if (searching) shown = searchModels([...all.filter((m) => !m.info.via), ...all.filter((m) => m.info.via)], query, favorites);
  else {
    shown = rowsOf(tab);
    const older = olderOf(tab);
    if (older && typeof tab !== "string") legacyRow = { provider: tab.provider, count: older, open: expanded.has(tab.provider) };
  }
  // The list keeps the height of its tallest tab, so switching providers never resizes the popup
  // (T3's `tallestTabUnits`); past `LIST_MAX` it scrolls.
  // The sidebar's rows are 1px apart, with a 9px divider after Favorites and Recent.
  const tallest = Math.max(1, ...tabs.map((t) => rowsOf(t).length + (olderOf(t) ? 1 : 0)));
  const lead = tabs.filter((t) => typeof t === "string").length;
  const sidebarHeight = sidebar ? tabs.length * (ROW + 1) + (lead ? 9 : 0) + 8 : 0;
  const bodyMin = Math.min(LIST_MAX, Math.max(tallest * ROW + 8, sidebarHeight));
  // Rows say their provider wherever providers mix.
  const mixed = searching || typeof tab === "string";
  const pickable = (m: PickerModel) => !unavailable(providerOf(m.provider)!);
  const jumps = shown.filter(pickable).slice(0, 9).map((m) => m.key);
  const shownKeys = [...shown.map((m) => m.key), ...(legacyRow ? [legacyKey(legacyRow.provider)] : [])];
  const hasOptions = (active?.options.length ?? 0) > 0;

  const focusOptions = () =>
    requestAnimationFrame(() => {
      const row = footer.current?.querySelector<HTMLElement>("[data-option-row]");
      if (row) rowControl(row)?.focus();
    });
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
      if (m.info.options.length) focusOptions();
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
    // A model with options keeps the popup open on them (T3's docked picker); one without closes it.
    if (m.info.options.length) focusOptions();
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
    if (sidebar && !searching && plain && ((e.key === "ArrowLeft" && !e.shiftKey && query === "") || (e.key === "Tab" && e.shiftKey))) {
      const bar = e.currentTarget.closest("[data-model-picker]")?.querySelector("[data-model-picker-sidebar]");
      const b = bar?.querySelector<HTMLButtonElement>('button[aria-pressed="true"]') ?? bar?.querySelector<HTMLButtonElement>("button");
      if (b) {
        e.preventDefault();
        b.focus();
        return;
      }
    }
    // Tab goes on to the picked model's options.
    if (e.key === "Tab" && !e.shiftKey && plain && hasOptions) {
      e.preventDefault();
      focusOptions();
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
  const notice = tabProvider && !searching ? providerNotice(tabProvider) : null;
  const updates = (searching ? providers : tabProvider ? [tabProvider] : []).map((p) => updateNotice(p, searching ? query : "")).filter((n): n is string => !!n);

  return (
    <div data-model-picker className="flex max-h-[min(var(--available-height),26rem)] w-[min(24rem,var(--available-width))] flex-col">
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
        <ComboboxSearchInput ref={search} autoFocus placeholder="Search models" aria-label="Search models" value={query} onChange={(e) => setQuery(e.target.value)} onKeyDown={onSearchKey} />
        {/* As tall as the tallest tab whatever is shown, so a search or another tab never resizes it. */}
        <div className="flex min-h-0 flex-1" style={{ minHeight: bodyMin }}>
          {sidebar && !searching && (
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
          <div className="flex min-h-0 min-w-0 flex-1 flex-col">
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
              {shown.map((m, i) => {
                const p = providerOf(m.provider)!;
                const jump = jumps.indexOf(m.key);
                const via = byKey.get(m.key)!.info.via;
                // In a provider's tab, an endpoint's models sit under one label, not a tag on each row.
                const label = !mixed && via && via !== byKey.get(shown[i - 1]?.key ?? "")?.info.via;
                return (
                  <Fragment key={m.key}>
                    {label && (
                      <div aria-hidden className="truncate px-2 pt-2 pb-1 text-[11px] text-subtle" title={`Listed by the endpoint ${via}`}>
                        via {via}
                      </div>
                    )}
                    <ModelRow
                      model={m}
                      info={byKey.get(m.key)!.info}
                      provider={p}
                      mixed={mixed}
                      favorite={favorites.has(m.key)}
                      selected={multi?.selected && multi.selected.length > 1 ? multi.selected.some((c) => modelKey(c.provider, c.model ?? "") === m.key) : m.key === activeKey}
                      starOnHover={tab === "favorites" && !searching}
                      jump={jump >= 0 ? jumpLabel(jump) : null}
                    />
                  </Fragment>
                );
              })}
              {legacyRow && (
                <ComboboxItem value={legacyKey(legacyRow.provider)} className="group w-full text-muted data-highlighted:text-fg">
                  <ChevronRight aria-hidden className={cn("size-3.5 transition-transform duration-150 motion-reduce:transition-none", legacyRow.open && "rotate-90")} />
                  <span>{legacyRow.open ? "Show fewer" : `${legacyRow.count} older`}</span>
                </ComboboxItem>
              )}
            </ComboboxList>
            <ComboboxEmpty className="py-4">{tab === "favorites" && !searching ? "No favorites yet" : notice ? null : "No models found"}</ComboboxEmpty>
            {(notice || updates.length > 0) && (
              <div className="max-h-28 shrink-0 space-y-1.5 overflow-y-auto border-t border-line px-3 py-2 text-[11.5px] leading-snug text-muted" data-model-picker-notices>
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
        </div>
      </Combobox>
      {active && hasOptions && (
        <div ref={footer} data-model-picker-options className="shrink-0 border-t border-line p-1" onKeyDownCapture={(e) => moveBetweenRows(e, search.current)}>
          {/* Whose options these are, said only when the picked model isn't in sight (a search, another tab). */}
          {activeKey && !shown.some((m) => m.key === activeKey) && (
            <p className="truncate px-2 pt-1 text-[11px] text-subtle">
              {active.short_name ?? active.name}
              {activeInfo && ` · ${activeInfo.name}`}
            </p>
          )}
          <ModelOptionsRows model={active} value={value} onEffort={setEffort} onOption={setOption} />
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

/**
 * The tabs (T3's `ModelPickerSidebar`), each labelled: Favorites, Recent, then every provider by
 * its logo and name. The shown one is a quiet fill, as a selected row anywhere on the surface.
 */
function Sidebar({ tabs, tab, providers, onSelect, onFocusSearch }: { tabs: Tab[]; tab: Tab; providers: readonly ProviderInfo[]; onSelect: (t: Tab) => void; onFocusSearch: () => void }) {
  const box = useRef<HTMLDivElement>(null);
  const lead = tabs.filter((t) => typeof t === "string").length;
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
        "flex h-7 w-full shrink-0 cursor-pointer items-center gap-2 rounded-sm px-2 text-left text-[12.5px] text-muted outline-none transition-colors hover:bg-fg/[0.05] hover:text-fg focus-visible:bg-fg/[0.08] focus-visible:text-fg aria-pressed:bg-fg/[0.07] aria-pressed:text-fg [&_svg]:shrink-0",
        extra?.dim && "text-subtle",
      )}
    >
      {icon}
      <span className="min-w-0 flex-1 truncate">{label}</span>
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
      className="flex w-40 shrink-0 flex-col gap-px overflow-y-auto overscroll-contain border-e border-line p-1 [scrollbar-width:none]"
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
      {tabs.includes("favorites") && button("favorites", "Favorites", <Star className="size-3.5" />)}
      {tabs.includes("recent") && button("recent", "Recent", <Clock className="size-3.5" />)}
      {lead > 0 && <div className="mx-2 my-1 h-px shrink-0 bg-line" aria-hidden />}
      {providers.map((p) => button({ provider: p.id }, p.name, <ProviderLogo provider={p} className="size-3.5" />, status(p)))}
    </div>
  );
}

function ModelRow({
  model,
  info,
  provider,
  mixed,
  favorite,
  selected,
  starOnHover,
  jump,
}: {
  model: PickerModel;
  info: ModelInfo;
  provider: ProviderInfo;
  /** Providers mix here (Favorites, Recent, a search): the row names its own. */
  mixed: boolean;
  favorite: boolean;
  selected: boolean;
  starOnHover: boolean;
  jump: string | null;
}) {
  const label = favorite ? "Remove from favorites" : "Add to favorites";
  const why = unavailable(provider);
  const limit = why ?? (provider.auth === "missing" ? "Log in first" : exhausted(provider.usage_limits) ? "Limit reached" : null);
  return (
    <ComboboxItem
      value={model.key}
      disabled={!!why}
      data-model-key={model.key}
      className="group relative w-full min-w-0 data-disabled:cursor-not-allowed data-disabled:opacity-60"
      aria-selected={selected}
      title={why ? `${provider.name}: ${why}` : limit === "Limit reached" ? (usageSummary(provider, Date.now()) ?? undefined) : undefined}
    >
      {mixed && <ProviderLogo provider={provider} className="size-3.5 text-muted" />}
      <span className="flex min-w-0 flex-1 items-center gap-1.5">
        {/* The name keeps its room; a "via" tag gives way first. */}
        <span className="min-w-0 max-w-full shrink-0 truncate leading-snug" title={model.name}>
          {model.name}
          <span className="sr-only">, {model.providerName}</span>
        </span>
        {/* T3's "New" chip, after the name and before what limits it. */}
        {info.badge === "new" && (
          <span aria-label="New model" className="shrink-0 rounded border border-accent/40 px-1 text-[10px] leading-4 font-medium tracking-wide text-accent uppercase">
            New
          </span>
        )}
        {info.custom && <span className="shrink-0 rounded border border-line-2 px-1 text-[10px] leading-4 text-subtle">Custom</span>}
        {!mixed && info.images === false && <span className="shrink-0 text-[11px] text-subtle">Text only</span>}
        {/* One line of where it comes from: the provider, what it can't take, then the endpoint that lists it. */}
        {mixed && (
          <span aria-hidden className="min-w-0 shrink truncate text-[11px] text-subtle" title={info.via ? `${provider.name}, listed by the endpoint ${info.via}` : undefined}>
            {[provider.name, info.images === false ? "Text only" : null, info.via ? `via ${info.via}` : null].filter(Boolean).join(" · ")}
          </span>
        )}
      </span>
      {limit && <span className="shrink-0 text-[11px] text-subtle">{limit}</span>}
      <span className="flex shrink-0 items-center gap-1.5">
        {jump && !limit && !selected && <kbd className="font-mono text-[10px] text-subtle opacity-0 group-hover:opacity-100 group-data-highlighted:opacity-100 max-sm:hidden [@media(hover:none)]:hidden">{jump}</kbd>}
        <button
          type="button"
          aria-label={label}
          title={label}
          className="grid size-4 place-items-center rounded text-subtle outline-none hover:text-fg focus-visible:ring-2 focus-visible:ring-accent/60"
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
              favorite && "fill-current text-fg",
              (!favorite || starOnHover) && "opacity-0 group-hover:opacity-100 group-data-highlighted:opacity-100 in-focus-visible:opacity-100 [@media(hover:none)]:opacity-100",
            )}
          />
        </button>
        <Check className={cn("size-3.5", !selected && "invisible")} aria-hidden />
      </span>
    </ComboboxItem>
  );
}

const OPTION_ROW = "flex h-8 w-full min-w-0 items-center gap-3 rounded-sm px-2 text-[12.5px]";
const OPTION_LABEL = "min-w-0 flex-1 truncate text-muted";

/** Up and Down move between the options' rows (T3's `moveBetweenRows`); Up from the first goes back to the search. */
function moveBetweenRows(e: KeyboardEvent<HTMLElement>, search: HTMLInputElement | null) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  if ((e.target as HTMLElement).closest("[data-option-menu]")) return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-option-row]")];
  const i = rows.findIndex((r) => r.contains(document.activeElement));
  if (i < 0) return;
  e.preventDefault();
  e.stopPropagation();
  const to = i + (e.key === "ArrowDown" ? 1 : -1);
  if (to < 0) return search?.focus();
  rowControl(rows[Math.min(to, rows.length - 1)])?.focus();
}

/** Where focus goes in an option's row: the picked segment, else its control. */
const rowControl = (row: HTMLElement) => row.querySelector<HTMLElement>("button[data-pressed]") ?? row.querySelector<HTMLElement>("button, [role=switch]");

/** Labels short enough to sit side by side in a row (Low · High, Standard · Fast); more takes a menu. */
const fitsSegmented = (labels: string[]) => labels.length <= 4 && labels.join("").length <= 22;

/**
 * The picked model's options, one compact row each, only what it declares (`ModelInfo.options`):
 * its reasoning levels, its speed, then the rest. A few short choices are a segmented control; more
 * open a menu (T3's `OptionMenuRow`); on or off is a switch.
 */
function ModelOptionsRows({ model, value, onEffort, onOption }: { model: ModelInfo; value: ModelChoice; onEffort: (effort: string) => void; onOption: (id: string, v: OptionValue) => void }) {
  const reasoning = reasoningOf(model);
  const speed = speedOf(model);
  const others = otherOptions(model);
  return (
    <>
      {reasoning && <ChoiceRow option={reasoning} value={effortValue(model, value.effort)} onChange={onEffort} />}
      {speed &&
        (speed.type === "boolean" ? (
          <SwitchRow option={speed} value={optionValue(speed, value.options) === true} onChange={(v) => onOption(speed.id, v)} />
        ) : (
          <ChoiceRow option={speed} value={optionValue(speed, value.options) as string | undefined} onChange={(v) => onOption(speed.id, v)} fast />
        ))}
      {others.map((o) =>
        o.type === "boolean" ? (
          <SwitchRow key={o.id} option={o} value={optionValue(o, value.options) === true} onChange={(v) => onOption(o.id, v)} />
        ) : (
          <ChoiceRow key={o.id} option={o} value={optionValue(o, value.options) as string | undefined} onChange={(v) => onOption(o.id, v)} />
        ),
      )}
    </>
  );
}

function ChoiceRow({ option, value, onChange, fast = false }: { option: SelectOption; value: string | undefined; onChange: (v: string) => void; fast?: boolean }) {
  const picked = value ?? option.default ?? option.choices[0]?.id ?? "";
  if (fitsSegmented(option.choices.map((c) => c.label))) {
    return (
      <div data-option-row className={OPTION_ROW} title={option.description}>
        <span className={OPTION_LABEL}>{option.label}</span>
        <Segmented
          label={option.label}
          value={picked}
          onChange={(v) => v !== value && onChange(v)}
          className="shrink-0 [&_button]:h-5.5 [&_button]:px-2 [&_button]:text-[12px]"
          options={option.choices.map((c) => ({
            value: c.id,
            title: [c.description, c.id === option.default ? "Default" : null].filter(Boolean).join(" · ") || undefined,
            label: (
              <>
                {fast && isFast(option, c.id) && <Zap aria-hidden className="fill-current" />}
                {c.label}
              </>
            ),
          }))}
        />
      </div>
    );
  }
  const current = option.choices.find((c) => c.id === picked)?.label ?? "";
  return (
    <div data-option-row className={OPTION_ROW}>
      <span className={OPTION_LABEL}>{option.label}</span>
      <Menu modal={false}>
        <MenuTrigger
          render={
            <button
              type="button"
              aria-label={`${option.label}: ${current}`}
              title={option.description}
              className="flex h-6.5 min-w-0 cursor-pointer items-center gap-1 rounded-lg bg-fg/[0.06] px-2.5 text-[12px] text-fg outline-none hover:bg-fg/[0.09] focus-visible:ring-2 focus-visible:ring-accent/60 data-popup-open:bg-fg/[0.09]"
            />
          }
        >
          <span className="max-w-36 truncate">{current}</span>
          <ChevronDown aria-hidden className="size-3 shrink-0 text-subtle" />
        </MenuTrigger>
        <MenuPopup side="top" align="end" sideOffset={4} className="w-56" data-option-menu>
          <MenuRadioGroup aria-label={option.label} value={picked} onValueChange={(v) => typeof v === "string" && v !== value && onChange(v)}>
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
    </div>
  );
}

/** On or off toggles in place (T3's `OptionSwitchRow`). */
function SwitchRow({ option, value, onChange }: { option: BooleanOption; value: boolean; onChange: (v: boolean) => void }) {
  return (
    <label data-option-row className={cn(OPTION_ROW, "cursor-pointer hover:bg-fg/[0.04]")} title={option.description}>
      <span className={OPTION_LABEL}>{option.label}</span>
      <Switch size="sm" checked={value} onCheckedChange={onChange} aria-label={option.label} />
    </label>
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
/** Where a picker's focus goes when it closes: the composer's text after a pick (T3's `scheduleComposerFocus`). */
export type FinalFocus = (closeType: string) => boolean | HTMLElement | null | void;

export function PermissionsMenu({
  value,
  onChange,
  side = "top",
  finalFocus,
}: {
  value: RuntimeMode;
  onChange: (mode: RuntimeMode) => void;
  side?: "top" | "bottom";
  finalFocus?: FinalFocus;
}) {
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
      <MenuPopup side={side} align="start" className="w-72" finalFocus={finalFocus}>
        <MenuRadioGroup value={value} onValueChange={(v) => onChange(v as RuntimeMode)}>
          {runtimeModes.map((r) => (
            <MenuRadioItem key={r.id} value={r.id} closeOnClick className="items-start py-1.5">
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
