"use client";

/**
 * The builder (docs/code/automations-ui.md): one vertical flow of cards, When → Only if → steps,
 * joined by hairline connectors with a `+` that inserts a step there (Zapier's editor, Linear
 * Loops' order). One step is open at a time; closed ones read as one line. A test's verdicts sit on
 * the cards they're about (the trigger, each condition, each step) and what each template rendered
 * to shows under its field.
 */

import {
  ArrowDown,
  ArrowUp,
  Check,
  ChevronDown,
  Copy,
  KeyRound,
  ListFilter as FilterIcon,
  MoreHorizontal,
  Plus,
  RefreshCw,
  Trash2,
  TriangleAlert,
  X,
} from "lucide-react";
import { Popover as PopoverPrimitive } from "@base-ui/react/popover";
import { useId, useMemo, useRef, useState, type KeyboardEvent, type ReactNode } from "react";
import type { Automation, AutomationSpec, Catalog, Filter, FilterOp, Step, StepInfo, TestResult, Trigger } from "@/lib/automations-api";
import {
  AGENT_EVENTS,
  CODE_EVENTS,
  cronShape,
  cronText,
  defaultTrigger,
  everyText,
  filterOps,
  matchVariables,
  newStep,
  problemsIn,
  schemaOf,
  stepInfo,
  triggerInfo,
  triggerSource,
  variablesAt,
  type Problems,
  type Schema,
  type Variable,
} from "@/lib/automations-ui";
import { field as fieldClass, openSettings } from "../settings-ui";
import { cn, Combobox, ComboboxItem, ComboboxList, ComboboxSearchInput, Menu, MenuGroup, MenuGroupLabel, MenuItem, MenuPopup, MenuSeparator, MenuTrigger, popupMotion, Segmented, Select } from "../ui";
import { useLayer } from "../ui/layer";
import { LookupPicker } from "../lookup-picker";
import { Picker, type PickerOption } from "../ui/picker";
import { useDismiss } from "../dismiss";
import { Pop } from "../pop";
import { useAgents } from "../use-agents";
import { AddButton, Chips, ConfigForm } from "./config-form";
import { NumberField, stepIcon, Tile, triggerIcon, SizedSelect } from "./shared";

export interface BuilderTest {
  result: TestResult | null;
  /** The event's data, for the variable picker's sample values. */
  data: unknown;
}

export function Builder({
  draft,
  onChange,
  catalog,
  problems,
  automation,
  revealed,
  onRotate,
  test,
  attempts = 0,
}: {
  draft: AutomationSpec;
  onChange: (next: AutomationSpec) => void;
  catalog: Catalog;
  problems: Problems;
  /** The saved automation (unset while it's new): its webhook and secrets. */
  automation: Automation | null;
  /** A webhook secret just made, shown once. */
  revealed: string | null;
  onRotate?: () => void;
  test: BuilderTest | null;
  /** Saves tried that had something to fix: what a new step lacks shows from then on. */
  attempts?: number;
}) {
  const [open, setOpen] = useState<string | null>(() => (draft.steps.length === 1 || !draft.steps.length ? (draft.steps[0]?.id ?? null) : null));
  // Steps just added: not red before they're filled in or a save is tried (they started red).
  const [fresh, setFresh] = useState<ReadonlySet<string>>(new Set());
  const [seenAttempts, setSeenAttempts] = useState(attempts);
  if (attempts !== seenAttempts) {
    setSeenAttempts(attempts);
    setFresh(new Set());
  }
  const root = useRef<HTMLDivElement>(null);
  const shown = useMemo(() => {
    if (!fresh.size) return problems;
    const hide = draft.steps.flatMap((s, i) => (fresh.has(s.id) ? [`steps.${i}`] : []));
    return Object.fromEntries(Object.entries(problems).filter(([k]) => !hide.some((h) => k === h || k.startsWith(`${h}.`))));
  }, [problems, fresh, draft.steps]);
  // A new step comes into view (it was added below the fold).
  const reveal = (id: string) =>
    requestAnimationFrame(() =>
      root.current?.querySelector(`[data-step="${CSS.escape(id)}"]`)?.scrollIntoView?.({ block: "nearest", behavior: window.matchMedia?.("(prefers-reduced-motion: reduce)").matches ? "auto" : "smooth" }),
    );
  const sample = useMemo(() => sampler(test?.data), [test?.data]);
  const setSteps = (steps: Step[]) => onChange({ ...draft, steps });
  const keys = useStepKeys(draft.steps.map((x) => x.id));
  const insert = (at: number, kind: string) => {
    const s = newStep(kind, catalog, draft.steps.map((x) => x.id));
    const steps = [...draft.steps];
    steps.splice(at, 0, s);
    setSteps(steps);
    setOpen(s.id);
    setFresh((f) => new Set(f).add(s.id));
    reveal(s.id);
  };
  const move = (i: number, by: number) => {
    const j = i + by;
    if (j < 0 || j >= draft.steps.length) return;
    const steps = [...draft.steps];
    [steps[i], steps[j]] = [steps[j], steps[i]];
    setSteps(steps);
  };
  return (
    <div ref={root} className="mx-auto w-full max-w-[620px] pb-4" aria-label="Builder">
      <TriggerCard draft={draft} onChange={onChange} catalog={catalog} problems={problems} automation={automation} revealed={revealed} onRotate={onRotate} verdict={test?.result ? test.result.trigger : null} />
      <Connector />
      <FiltersCard filters={draft.filters} onChange={(filters) => onChange({ ...draft, filters })} variables={variablesAt(draft, catalog, -1)} problems={problems} verdicts={test?.result?.filters ?? null} sample={sample} />
      {draft.steps.map((s, i) => (
        <div key={keys[i]} data-step={s.id}>
          <Connector onAdd={(kind) => insert(i, kind)} catalog={catalog} label={`Add a step before ${s.name || s.id}`} />
          <StepCard
            step={s}
            index={i}
            count={draft.steps.length}
            draft={draft}
            catalog={catalog}
            problems={shown}
            open={open === s.id}
            onToggle={() => setOpen(open === s.id ? null : s.id)}
            onChange={(next) => {
              setSteps(draft.steps.map((x, j) => (j === i ? next : x)));
              if (fresh.has(s.id)) setFresh((f) => new Set([...f].filter((x) => x !== s.id)));
              if (next.id !== s.id && open === s.id) setOpen(next.id);
            }}
            onMove={(by) => move(i, by)}
            onRemove={() => setSteps(draft.steps.filter((_, j) => j !== i))}
            onDuplicate={() => {
              const copy = { ...structuredClone(s), id: newStep(s.kind, catalog, draft.steps.map((x) => x.id)).id, name: s.name ? `${s.name} (copy)` : undefined };
              const steps = [...draft.steps];
              steps.splice(i + 1, 0, copy);
              setSteps(steps);
              setOpen(copy.id);
              reveal(copy.id);
            }}
            secrets={automation?.secrets ?? []}
            sample={sample}
            tested={test?.result?.steps.find((x) => x.id === s.id) ?? null}
          />
        </div>
      ))}
      <Connector />
      <div className="flex justify-center">
        <AddStepMenu catalog={catalog} onPick={(kind) => insert(draft.steps.length, kind)} trigger={
          <span className={cn("inline-flex h-8 items-center gap-1.5 rounded-lg border border-dashed px-3 text-[12.5px] font-[510] transition-colors", problems.steps ? "border-danger/50 text-danger" : "border-line-3 text-muted hover:border-subtle hover:text-fg")}>
            <Plus className="size-3.5" /> {draft.steps.length ? "Add a step" : "Add the first step"}
          </span>
        } />
      </div>
      {problems.steps && <p className="mt-2 text-center text-[11.5px] text-danger">{problems.steps}</p>}
    </div>
  );
}

/** A value from the test event for a variable path (`event.data.x.y`). */
function sampler(data: unknown) {
  return (path: string): string | undefined => {
    if (data === undefined || !path.startsWith("event.data.")) return undefined;
    let cur: unknown = data;
    for (const p of path.slice(11).split(".")) {
      if (cur === null || typeof cur !== "object") return undefined;
      cur = (cur as Record<string, unknown>)[p];
    }
    if (cur === undefined) return undefined;
    const s = typeof cur === "string" ? cur : JSON.stringify(cur);
    return s.length > 40 ? `${s.slice(0, 40)}…` : s;
  };
}

// ── the frame ──

function FlowCard({ children, tone, label }: { children: ReactNode; tone?: "danger" | "ok" | "muted" | "accent"; label: string }) {
  return (
    <section
      aria-label={label}
      className={cn(
        "rounded-xl border bg-panel shadow-card inset-shadow-highlight transition-[border-color,box-shadow] duration-150",
        tone === "danger" ? "border-danger/45" : tone === "ok" ? "border-ok/40" : tone === "accent" ? "border-accent/40" : "border-line-2",
        tone === "muted" && "opacity-70",
      )}
    >
      {children}
    </section>
  );
}

function CardHead({ kicker, title, icon, children, onClick, open }: { kicker: string; title: ReactNode; icon: ReactNode; children?: ReactNode; onClick?: () => void; open?: boolean }) {
  const body = (
    <>
      {icon}
      <span className="min-w-0 flex-1 text-left">
        <span className="block text-[10.5px] font-medium tracking-[0.06em] text-subtle uppercase">{kicker}</span>
        <span className="block truncate text-[13.5px] font-[590] tracking-[-0.015em]">{title}</span>
      </span>
    </>
  );
  return (
    <div className="flex min-h-14 items-center gap-2.5 px-3.5 py-2.5">
      {onClick ? (
        <button type="button" onClick={onClick} aria-expanded={open} className="-my-1 flex min-w-0 flex-1 items-center gap-2.5 rounded-md py-1 outline-none focus-visible:ring-2 focus-visible:ring-accent/60">
          {body}
        </button>
      ) : (
        <span className="flex min-w-0 flex-1 items-center gap-2.5">{body}</span>
      )}
      {children}
    </div>
  );
}

function Verdict({ ok, label }: { ok: boolean; label?: string }) {
  return (
    <span className={cn("inline-flex h-5 shrink-0 items-center gap-1 rounded-full border px-1.5 text-[11px] font-medium", ok ? "border-ok/35 bg-ok/10 text-ok" : "border-line-2 bg-fg/[0.03] text-muted")}>
      {ok ? <Check className="size-3" strokeWidth={2.25} /> : <X className="size-3" strokeWidth={2.25} />}
      {label ?? (ok ? "Passes" : "Doesn't pass")}
    </span>
  );
}

function Connector({ onAdd, catalog, label }: { onAdd?: (kind: string) => void; catalog?: Catalog; label?: string }) {
  return (
    <div className="group/conn relative flex h-6 justify-center" aria-hidden={!onAdd}>
      <span className="h-full w-px bg-line-3" />
      {onAdd && catalog && (
        <span className="absolute top-1/2 left-1/2 -translate-1/2 opacity-0 transition-opacity duration-150 group-hover/conn:opacity-100 focus-within:opacity-100 [@media(hover:none)]:opacity-100">
          <AddStepMenu
            catalog={catalog}
            onPick={onAdd}
            label={label}
            trigger={
              <span className="grid size-5 place-items-center rounded-full border border-line-3 bg-sheet text-muted shadow-card transition-colors hover:border-subtle hover:text-fg">
                <Plus className="size-3" strokeWidth={2} />
              </span>
            }
          />
        </span>
      )}
    </div>
  );
}

const GROUPS: { label: string; kinds: string[] }[] = [
  { label: "Agents", kinds: ["agent.start", "agent.wait", "agent.message", "agent.respond", "agent.publish", "agent.script", "agent.settle"] },
  { label: "Send and wait", kinds: ["notify", "webhook", "delay"] },
];

/** The catalog's steps in the menu's order: agents, send and wait, then each integration. */
function stepSections(catalog: Catalog): { label: string; steps: StepInfo[] }[] {
  const known = (kinds: string[]) => kinds.map((k) => stepInfo(catalog, k)).filter((s): s is StepInfo => !!s);
  const sections = GROUPS.map((g) => ({ label: g.label, steps: known(g.kinds) }));
  const byIntegration = new Map<string, StepInfo[]>();
  for (const s of catalog.steps) {
    if (GROUPS.some((g) => g.kinds.includes(s.kind))) continue;
    const k = s.integration ?? "Other";
    byIntegration.set(k, [...(byIntegration.get(k) ?? []), s]);
  }
  for (const [name, steps] of byIntegration) sections.push({ label: name[0].toUpperCase() + name.slice(1), steps });
  return sections.filter((s) => s.steps.length > 0);
}

/** Steps matching every word typed (title, description, kind, integration: not a group label like "Send and wait"); titles that start with it first. */
function matchSteps(sections: { label: string; steps: StepInfo[] }[], query: string): { group: string; step: StepInfo }[] {
  const all = sections.flatMap((g) => g.steps.map((step) => ({ group: g.label, step })));
  const words = query.toLowerCase().split(/\s+/).filter(Boolean);
  if (!words.length) return all;
  const rank = ({ step }: { step: StepInfo }) => {
    const title = step.title.toLowerCase();
    const hay = `${title} ${step.description} ${step.kind} ${step.integration ?? ""}`.toLowerCase();
    if (!words.every((w) => hay.includes(w))) return -1;
    return title.startsWith(words[0]) ? 0 : words.every((w) => title.includes(w)) ? 1 : 2;
  };
  return all
    .map((r) => ({ r, rank: rank(r) }))
    .filter((x) => x.rank >= 0)
    .sort((a, b) => a.rank - b.rank)
    .map((x) => x.r);
}

/**
 * "Add a step": a searchable list of the catalog's steps, like ⌘K (type to filter, ↑↓ to move, ↵ to add). It closes on a
 * pick, Escape or a press outside, never on a blur alone (Safari doesn't focus a clicked row).
 */
function AddStepMenu({ catalog, onPick, trigger, label = "Add a step" }: { catalog: Catalog; onPick: (kind: string) => void; trigger: ReactNode; label?: string }) {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const search = useRef<HTMLInputElement>(null);
  const highlighted = useRef<string | null>(null);
  const sections = useMemo(() => stepSections(catalog), [catalog]);
  const rows = useMemo(() => matchSteps(sections, query), [sections, query]);
  const keys = rows.map((r) => r.step.kind);
  const listId = useId();
  const layer = useLayer();
  const pick = (kind: string) => {
    setOpen(false);
    setQuery("");
    onPick(kind);
  };
  const onSearchKey = (e: KeyboardEvent<HTMLInputElement>) => {
    if (e.key !== "Enter" || e.nativeEvent.isComposing) return;
    const kind = highlighted.current && keys.includes(highlighted.current) ? highlighted.current : keys[0];
    if (!kind) return;
    (e as typeof e & { preventBaseUIHandler?: () => void }).preventBaseUIHandler?.();
    e.preventDefault();
    pick(kind);
  };
  return (
    <PopoverPrimitive.Root
      open={open}
      onOpenChange={(next) => {
        setOpen(next);
        if (!next) setQuery("");
      }}
    >
      <PopoverPrimitive.Trigger aria-label={label} title={label} className="rounded-lg outline-none focus-visible:ring-2 focus-visible:ring-accent/60">
        {trigger}
      </PopoverPrimitive.Trigger>
      <PopoverPrimitive.Portal {...layer}>
        <PopoverPrimitive.Positioner className="z-[130]" side="bottom" align="start" sideOffset={4} collisionPadding={{ top: 56, right: 8, bottom: 8, left: 8 }} collisionAvoidance={{ side: "flip", align: "shift", fallbackAxisSide: "none" }}>
          <PopoverPrimitive.Popup
            aria-label={label}
            // Opened by touch, the popup takes focus so a phone's keyboard stays down until the search is tapped.
            initialFocus={(type) => (type === "touch" ? true : search.current)}
            className={cn("rounded-lg text-fg outline-none", popupMotion)}
          >
            <div className="flex max-h-[min(28rem,var(--available-height))] w-80 max-w-(--available-width) flex-col rounded-lg border border-outline bg-popover shadow-pop inset-shadow-highlight">
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
                  if (typeof k === "string") pick(k);
                }}
              >
                <ComboboxSearchInput ref={search} aria-label="Search steps" aria-controls={listId} placeholder="Search steps…" value={query} onChange={(e) => setQuery(e.target.value)} onKeyDown={onSearchKey} />
                <ComboboxList id={listId} aria-label="Steps" className="max-h-96">
                  {rows.map(({ group, step }, i) => {
                    const Icon = stepIcon(step.kind);
                    // Headings only while the list is in its own order; a search is ranked, so its rows say their group.
                    const heading = !query.trim() && (i === 0 || rows[i - 1].group !== group) ? group : null;
                    return (
                      <div key={step.kind} role="presentation">
                        {heading && <div className={cn("px-2 pb-1 text-[11px] font-medium text-subtle", i > 0 ? "mt-1 border-t border-line pt-2" : "pt-1")}>{heading}</div>}
                        <ComboboxItem value={step.kind} className="w-full items-start py-1.5">
                          <Icon className="mt-0.5 size-3.5" strokeWidth={1.75} />
                          <span className="min-w-0 flex-1">
                            <span className="flex items-center gap-1.5">
                              {step.title}
                              {step.needs_setup && <span className="rounded-[4px] border border-line-2 px-1 text-[10px] leading-[15px] text-subtle">Connect</span>}
                              {query.trim() && <span className="ml-auto shrink-0 text-[10.5px] text-subtle">{group}</span>}
                            </span>
                            <span className="line-clamp-2 text-[11.5px] leading-snug text-pretty text-subtle">{plainText(step.description)}</span>
                          </span>
                        </ComboboxItem>
                      </div>
                    );
                  })}
                </ComboboxList>
                {rows.length === 0 && <p className="px-3 py-5 text-center text-[12.5px] text-subtle">No step matches “{query.trim()}”</p>}
              </Combobox>
            </div>
          </PopoverPrimitive.Popup>
        </PopoverPrimitive.Positioner>
      </PopoverPrimitive.Portal>
    </PopoverPrimitive.Root>
  );
}

// ── the trigger ──

function TriggerCard({
  draft,
  onChange,
  catalog,
  problems,
  automation,
  revealed,
  onRotate,
  verdict,
}: {
  draft: AutomationSpec;
  onChange: (next: AutomationSpec) => void;
  catalog: Catalog;
  problems: Problems;
  automation: Automation | null;
  revealed: string | null;
  onRotate?: () => void;
  verdict: boolean | null;
}) {
  const t = draft.trigger;
  const info = triggerInfo(catalog, t);
  const set = (trigger: Trigger) => onChange({ ...draft, trigger });
  const bad = problemsIn(problems, "trigger");
  return (
    <FlowCard label="Trigger" tone={bad.length ? "danger" : undefined}>
      <CardHead kicker="When" title={info?.title ?? triggerSource(t)} icon={<Tile icon={triggerIcon(triggerSource(t))} tone="accent" />}>
        {verdict !== null && <Verdict ok={verdict} label={verdict ? "Matches" : "Doesn't match"} />}
        <Menu>
          <MenuTrigger className="inline-flex h-7 items-center gap-1 rounded-md px-2 text-[12px] text-muted transition-colors hover:bg-fg/[0.06] hover:text-fg data-popup-open:bg-fg/[0.06]" aria-label="Change the trigger">
            Change <ChevronDown className="size-3" />
          </MenuTrigger>
          <MenuPopup align="end" className="w-[26rem]">
            {catalog.triggers.map((x) => {
              const Icon = triggerIcon(x.source);
              return (
                <MenuItem key={x.source} onClick={() => set(defaultTrigger(x.source, catalog))} className="items-start py-1.5">
                  <Icon className="mt-0.5 size-3.5" strokeWidth={1.75} />
                  <span className="min-w-0 flex-1">
                    <span className="flex items-center gap-1.5">
                      {x.title}
                      {x.needs_setup && <span className="rounded-[4px] border border-line-2 px-1 text-[10px] leading-[15px] text-subtle">Connect</span>}
                    </span>
                    <span className="line-clamp-2 text-[11.5px] leading-snug text-pretty text-subtle">{plainText(x.description)}</span>
                  </span>
                  {x.source === triggerSource(t) && <Check className="mt-0.5 size-3.5" />}
                </MenuItem>
              );
            })}
          </MenuPopup>
        </Menu>
      </CardHead>
      <div className="space-y-3 border-t border-line px-3.5 py-3.5">
        <TriggerBody trigger={t} onChange={set} info={info} problems={problems} automation={automation} revealed={revealed} onRotate={onRotate} name={draft.name} />
        {bad
          .filter(([k]) => k === "trigger")
          .map(([k, m]) => (
            <p key={k} className="text-[11.5px] text-danger">
              {m}
            </p>
          ))}
      </div>
    </FlowCard>
  );
}

const codeEventLabels: Record<string, string> = { landed: "Landed", change: "Changes", check: "Checks", review: "Reviews", queue: "Merge queue", work: "Work items", preview: "Previews", push: "Pushes", thread: "Discussions" };
const agentEventLabels: Record<string, string> = { "thread.created": "Started", "thread.done": "Done", "thread.failed": "Failed", "thread.waiting": "Waiting on you", "thread.settled": "Settled", "request.opened": "Asks" };

const PRESETS = [
  { value: "hourly", label: "Hourly" },
  { value: "daily", label: "Daily" },
  { value: "weekdays", label: "Weekdays" },
  { value: "weekly", label: "Weekly" },
  { value: "custom", label: "Custom" },
] as const;
type Preset = (typeof PRESETS)[number]["value"];

function presetOf(cron: string): { preset: Preset; time: string; day: string } {
  const p = cron.trim().split(/\s+/);
  const [m, h, dom, mon, dow] = p.length === 6 ? p.slice(1) : p;
  const time = /^\d+$/.test(m ?? "") && /^\d+$/.test(h ?? "") ? `${h.padStart(2, "0")}:${m.padStart(2, "0")}` : "09:00";
  if (p.length === 5 && dom === "*" && mon === "*") {
    if (/^\d+$/.test(m) && h === "*" && dow === "*") return { preset: "hourly", time, day: "1" };
    if (/^\d+$/.test(m) && /^\d+$/.test(h)) {
      if (dow === "*") return { preset: "daily", time, day: "1" };
      if (dow === "1-5") return { preset: "weekdays", time, day: "1" };
      if (/^[0-6]$/.test(dow)) return { preset: "weekly", time, day: dow };
    }
  }
  return { preset: "custom", time, day: "1" };
}

function cronFor(preset: Preset, time: string, day: string, was: string): string {
  const [h, m] = time.split(":").map((x) => String(Number(x)));
  switch (preset) {
    case "hourly":
      return `${m} * * * *`;
    case "daily":
      return `${m} ${h} * * *`;
    case "weekdays":
      return `${m} ${h} * * 1-5`;
    case "weekly":
      return `${m} ${h} * * ${day}`;
    case "custom":
      return was;
  }
}

/** A catalog description as a menu shows it: its `code` marks dropped (a one-line hint can't set them). */
const plainText = (s: string) => s.replace(/`/g, "");

function zones(current: string): string[] {
  let all: string[] = [];
  try {
    all = (Intl as unknown as { supportedValuesOf?: (k: string) => string[] }).supportedValuesOf?.("timeZone") ?? [];
  } catch {
    all = [];
  }
  const set = new Set(["UTC", current, ...all]);
  return [...set];
}

/** The zones as a searchable list (400-odd): named by their city with its UTC offset now, grouped by region, the full id searchable. */
function zoneOptions(current: string): PickerOption[] {
  zoneCache ??= new Map();
  const cached = zoneCache.get(current);
  if (cached) return cached;
  const now = new Date();
  // The picked zone and this browser's own come first (the list opened on Africa with Berlin picked).
  const local = Intl.DateTimeFormat().resolvedOptions().timeZone;
  const lead = [...new Set([current, local, "UTC"].filter(Boolean))];
  const options = [...lead, ...zones(current).filter((z) => !lead.includes(z)).sort((a, b) => a.localeCompare(b))]
    .map((z): PickerOption => {
      const cut = z.indexOf("/");
      const hint = offsetOf(z, now);
      const label = cut < 0 ? z : z.slice(cut + 1).replace(/_/g, " ").replace(/\//g, " · ");
      return lead.includes(z) || cut < 0 ? { id: z, label, key: z, hint } : { id: z, label, key: z, hint, group: z.slice(0, cut) };
    });
  zoneCache.set(current, options);
  return options;
}
let zoneCache: Map<string, PickerOption[]> | undefined;

/** "UTC+2", "UTC−3:30" for a zone right now; nothing if the browser can't say. */
function offsetOf(zone: string, at: Date): string | undefined {
  try {
    const part = new Intl.DateTimeFormat("en-US", { timeZone: zone, timeZoneName: "shortOffset" }).formatToParts(at).find((p) => p.type === "timeZoneName")?.value;
    return part?.replace(/^GMT/, "UTC").replace("-", "−").replace(/^UTC$/, "UTC±0");
  } catch {
    return undefined;
  }
}

function TriggerBody({
  trigger: t,
  onChange,
  info,
  problems,
  automation,
  revealed,
  onRotate,
  name,
}: {
  trigger: Trigger;
  onChange: (t: Trigger) => void;
  info: ReturnType<typeof triggerInfo>;
  problems: Problems;
  automation: Automation | null;
  revealed: string | null;
  onRotate?: () => void;
  name: string;
}) {
  const [custom, setCustom] = useState(false);
  switch (t.type) {
    case "manual":
      return (
        <p className="text-[12.5px] leading-relaxed text-muted">
          From the <b className="font-medium text-soft">Run</b> button here, <code className="font-mono text-[11.5px] text-soft">roll agent automation run {slugOf(name)}</code>, or the MCP tool. A prompt typed in is{" "}
          <code className="font-mono text-[11.5px] text-accent">{"{{ event.data.input.prompt }}"}</code>; JSON passed in is{" "}
          <code className="font-mono text-[11.5px] text-accent">{"{{ event.data.input }}"}</code> (its fields, <code className="font-mono text-[11.5px] text-accent">{"{{ event.data.input.<field> }}"}</code>).
        </p>
      );
    case "schedule": {
      const p = presetOf(t.cron);
      const preset: Preset = custom ? "custom" : p.preset;
      const words = cronText(t.cron);
      const err = problems["trigger.cron"] ?? (preset === "custom" ? cronShape(t.cron) : null);
      return (
        <div className="space-y-3">
          <Segmented label="How often" value={preset} onChange={(v) => (setCustom(v === "custom"), v !== "custom" && onChange({ ...t, cron: cronFor(v, p.time, p.day, t.cron) }))} options={PRESETS} />
          <div className="flex flex-wrap items-center gap-2">
            {preset === "weekly" && (
              <SizedSelect width="w-36"
                label="Day"
                value={p.day}
                onChange={(day) => onChange({ ...t, cron: cronFor("weekly", p.time, day, t.cron) })}
                options={["1", "2", "3", "4", "5", "6", "0"].map((d) => ({ value: d, label: ["Sundays", "Mondays", "Tuesdays", "Wednesdays", "Thursdays", "Fridays", "Saturdays"][Number(d)] }))}
               
              />
            )}
            {(preset === "daily" || preset === "weekdays" || preset === "weekly") && (
              <input type="time" aria-label="At" value={p.time} onChange={(e) => e.target.value && onChange({ ...t, cron: cronFor(preset, e.target.value, p.day, t.cron) })} className={cn(fieldClass, "w-28 font-mono text-[12.5px] [color-scheme:dark]")} />
            )}
            {preset === "hourly" && (
              <label className="flex items-center gap-2 text-[12.5px] text-muted">
                at minute
                <NumberField min={0} max={59} aria-label="Minute" value={Number(p.time.split(":")[1])} onChange={(n) => n !== null && onChange({ ...t, cron: `${n} * * * *` })} className={cn(fieldClass, "w-16 font-mono text-[12.5px]")} />
              </label>
            )}
            {preset === "custom" && <input aria-label="Cron" value={t.cron} onChange={(e) => onChange({ ...t, cron: e.target.value })} placeholder="*/30 9-17 * * 1-5" className={cn(fieldClass, "w-52 font-mono text-[12.5px]", err && "border-danger/50")} />}
            <Picker label="Time zone" searchPlaceholder="Find a city or zone…" recents="timezones" className="w-56" value={t.timezone} onChange={(timezone) => timezone && onChange({ ...t, timezone })} options={zoneOptions(t.timezone)} />
          </div>
          {err ? <p className="text-[11.5px] text-danger">{err}</p> : <p className="text-[11.5px] text-subtle">{words ? `${words}, ${t.timezone.replace(/_/g, " ")}` : <span className="font-mono">{t.cron}</span>}</p>}
        </div>
      );
    }
    case "interval": {
      const unit = t.every_s % 86_400 === 0 ? 86_400 : t.every_s % 3600 === 0 ? 3600 : 60;
      return (
        <div className="space-y-2">
          <div className="flex items-center gap-2">
            <span className="text-[12.5px] text-muted">Every</span>
            <NumberField min={1} aria-label="How many" value={Math.max(1, Math.round(t.every_s / unit))} onChange={(n) => n !== null && onChange({ ...t, every_s: n * unit })} className={cn(fieldClass, "w-20 font-mono text-[12.5px]")} />
            <Segmented
              label="Unit"
              value={String(unit)}
              onChange={(u) => onChange({ ...t, every_s: Math.max(1, Math.round(t.every_s / unit)) * Number(u) })}
              options={[
                { value: "60", label: "minutes" },
                { value: "3600", label: "hours" },
                { value: "86400", label: "days" },
              ]}
            />
          </div>
          {problems["trigger.every_s"] ? <p className="text-[11.5px] text-danger">{problems["trigger.every_s"]}</p> : <p className="text-[11.5px] text-subtle">{everyText(t.every_s)}, counted from when it's turned on.</p>}
        </div>
      );
    }
    case "webhook":
      return <WebhookBody automation={automation} revealed={revealed} onRotate={onRotate} />;
    case "code":
      return (
        <div className="space-y-3">
          <div>
            <p className="mb-1.5 text-[12.5px] font-[510] text-soft">Events</p>
            <Chips label="Events" options={CODE_EVENTS.map((e) => ({ value: e as string, label: codeEventLabels[e] ?? e }))} picked={new Set(t.events)} onChange={(events) => onChange({ ...t, events })} />
            {!t.events.length && <p className="mt-1 text-[11.5px] text-subtle">None picked: every event.</p>}
          </div>
          <div>
            <p className="mb-1.5 text-[12.5px] font-[510] text-soft">Repositories</p>
            <LookupPicker label="Repositories" multiple from={{ source: "code", kind: "repos" }} value={t.repos} onChange={(repos) => onChange({ ...t, repos })} placeholder="All your repos" custom />
          </div>
        </div>
      );
    case "agent":
      return (
        <div>
          <p className="mb-1.5 text-[12.5px] font-[510] text-soft">When a thread is</p>
          <Chips label="Agent events" options={AGENT_EVENTS.map((e) => ({ value: e, label: agentEventLabels[e] }))} picked={new Set(t.events)} onChange={(events) => onChange({ ...t, events })} />
        </div>
      );
    case "integration":
      return (
        <div className="space-y-3">
          {info?.needs_setup && (
            <p className="flex items-center gap-2 rounded-lg border border-line-2 bg-fg/[0.02] px-3 py-2 text-[12px] text-muted">
              <TriangleAlert className="size-3.5 shrink-0 text-subtle" />
              <span>
                Connect {info.title} in{" "}
                <button type="button" onClick={() => openSettings("owner", info.title)} className="text-fg underline-offset-2 hover:underline">
                  Agents settings
                </button>{" "}
                before this can run.
              </span>
            </p>
          )}
          {info && info.events.length > 0 && (
            <div>
              <p className="mb-1.5 text-[12.5px] font-[510] text-soft">Events</p>
              <Chips label="Events" options={info.events.map((e) => ({ value: e.kind, label: e.title }))} picked={new Set(t.events)} onChange={(events) => onChange({ ...t, events })} />
            </div>
          )}
          {info?.config != null && (
            <ConfigForm schema={info.config as Schema} value={(t.config as Record<string, unknown>) ?? {}} onChange={(config) => onChange({ ...t, config })} ctx={{ variables: [], problems, at: "trigger.config", secrets: [] }} />
          )}
        </div>
      );
  }
}

const slugOf = (name: string) => (/^[\w.-]+$/.test(name) ? name : `"${name || "name"}"`);

function WebhookBody({ automation, revealed, onRotate }: { automation: Automation | null; revealed: string | null; onRotate?: () => void }) {
  const hook = automation?.webhook;
  if (!hook) return <p className="text-[12.5px] text-muted">Save it to get its URL and signing secret. Requests are signed with HMAC-SHA256 and answered 202 once recorded.</p>;
  return (
    <div className="space-y-2.5">
      <CopyRow label="URL" value={hook.url} />
      {revealed ? (
        <div className="rounded-lg border border-accent/30 bg-accent/[0.06] p-2.5">
          <CopyRow label="Secret" value={revealed} />
          <p className="mt-1.5 text-[11.5px] text-muted">Copy it now: it&apos;s shown this once. roll keeps only a sealed copy.</p>
        </div>
      ) : (
        <div className="flex items-center gap-2">
          <span className="w-12 shrink-0 text-[12px] text-subtle">Secret</span>
          <span className="flex h-8 min-w-0 flex-1 items-center gap-2 rounded-lg border border-line-2 bg-panel-2 px-2.5 font-mono text-[12px] text-muted">
            <KeyRound className="size-3.5 shrink-0 text-subtle" />
            rlh_••••••••{hook.secret_hint}
          </span>
          {onRotate && (
            <button type="button" onClick={onRotate} className="inline-flex h-8 items-center gap-1.5 rounded-lg px-2 text-[12px] text-muted hover:bg-fg/[0.05] hover:text-fg">
              <RefreshCw className="size-3.5" /> New secret
            </button>
          )}
        </div>
      )}
      <p className="text-[11.5px] leading-relaxed text-subtle">
        Sign <code className="font-mono">{"{timestamp}.{body}"}</code> with it and send <code className="font-mono">X-Roll-Timestamp</code> and <code className="font-mono">X-Roll-Signature-256: sha256=…</code>. The body is{" "}
        <code className="font-mono text-accent">{"{{ event.data.body }}"}</code>.
      </p>
    </div>
  );
}

function CopyRow({ label, value }: { label: string; value: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <div className="flex items-center gap-2">
      <span className="w-12 shrink-0 text-[12px] text-subtle">{label}</span>
      <code className="flex h-8 min-w-0 flex-1 items-center truncate rounded-lg border border-line-2 bg-panel-2 px-2.5 font-mono text-[12px] text-soft" title={value}>
        <span className="truncate">{value}</span>
      </code>
      <button
        type="button"
        aria-label={`Copy the ${label.toLowerCase()}`}
        onClick={() => {
          void navigator.clipboard?.writeText(value);
          setCopied(true);
          setTimeout(() => setCopied(false), 1400);
        }}
        className="grid size-8 shrink-0 place-items-center rounded-lg text-muted transition-colors hover:bg-fg/[0.05] hover:text-fg"
      >
        {copied ? <Check className="size-3.5 text-ok" /> : <Copy className="size-3.5" />}
      </button>
    </div>
  );
}

// ── conditions ──

/** The condition menu: each op and its negation as one choice ("is", "is not"). */
const CONDITIONS: { value: string; op: FilterOp; not: boolean; label: string }[] = filterOps.flatMap((o) => [
  { value: o.op, op: o.op, not: false, label: o.label },
  {
    value: `not:${o.op}`,
    op: o.op,
    not: true,
    label: o.op === "exists" ? "is not set" : o.op === "equals" ? "is not" : o.op === "contains" ? "doesn't contain" : o.op === "has_label" ? "doesn't have label" : o.op === "any_of" ? "is none of" : o.op === "glob" ? "doesn't match path" : `is not ${o.label.replace(/^is /, "")}`,
  },
]);

export function FilterRows({ filters, onChange, variables, problems, at, verdicts, sample }: { filters: Filter[]; onChange: (f: Filter[]) => void; variables: readonly Variable[]; problems: Problems; at: string; verdicts: boolean[] | null; sample: (p: string) => string | undefined }) {
  return (
    <div className="space-y-2">
      {filters.map((f, i) => {
        const cond = CONDITIONS.find((c) => c.op === f.op && c.not === !!f.not) ?? CONDITIONS[0];
        const needs = filterOps.find((o) => o.op === f.op)?.needs ?? "value";
        const err = problems[`${at}.${i}`];
        const set = (patch: Partial<Filter>) => onChange(filters.map((x, j) => (j === i ? { ...x, ...patch } : x)));
        return (
          <div key={i} className="space-y-1">
            <div className="flex flex-wrap items-center gap-1.5">
              <span className="w-9 shrink-0 text-right text-[12px] text-subtle">{i === 0 ? "If" : "and"}</span>
              <PathInput value={f.field} onChange={(field) => set({ field })} variables={variables} invalid={!!err && !f.field} />
              <SizedSelect width="w-40 min-w-0" label="Condition" value={cond.value} onChange={(v) => {
                const c = CONDITIONS.find((x) => x.value === v)!;
                const nextNeeds = filterOps.find((o) => o.op === c.op)?.needs;
                set({ op: c.op, not: c.not || undefined, value: nextNeeds === "none" ? null : nextNeeds === "list" ? (Array.isArray(f.value) ? f.value : f.value ? [String(f.value)] : []) : Array.isArray(f.value) ? f.value.join(", ") : f.value });
              }} options={CONDITIONS.map((c) => ({ value: c.value, label: c.label }))} />
              {needs !== "none" && (
                <input
                  aria-label="Value"
                  value={Array.isArray(f.value) ? f.value.join(", ") : f.value == null ? "" : String(f.value)}
                  onChange={(e) => set({ value: needs === "list" ? e.target.value.split(",").map((x) => x.trim()).filter(Boolean) : needs === "number" ? (e.target.value === "" ? "" : Number(e.target.value)) : e.target.value })}
                  placeholder={needs === "list" ? "a, b, c" : f.op === "glob" ? "src/**/*.rs" : sample(f.field) ?? "value"}
                  className={cn(fieldClass, "min-w-24 flex-1 text-[12.5px]", (f.op === "glob" || needs === "number") && "font-mono", err && f.field && "border-danger/50")}
                />
              )}
              {verdicts && verdicts[i] !== undefined && <Verdict ok={verdicts[i]} label={verdicts[i] ? "Yes" : "No"} />}
              <button type="button" onClick={() => onChange(filters.filter((_, j) => j !== i))} aria-label={`Remove condition ${i + 1}`} className="grid size-7 shrink-0 place-items-center rounded-md text-subtle hover:bg-fg/[0.06] hover:text-fg">
                <X className="size-3.5" />
              </button>
            </div>
            {err && <p className="pl-11 text-[11.5px] text-danger">{err}</p>}
          </div>
        );
      })}
      <div className={filters.length ? "pl-10" : ""}>
        <AddButton onClick={() => onChange([...filters, { field: variables.find((v) => v.group === "Event")?.path ?? "event.kind", op: "equals", value: "" }])}>Add a condition</AddButton>
      </div>
    </div>
  );
}

/** A dotted path, with the variables as suggestions while typing. */
function PathInput({ value, onChange, variables, invalid }: { value: string; onChange: (v: string) => void; variables: readonly Variable[]; invalid?: boolean }) {
  const [open, setOpen] = useState(false);
  const [index, setIndex] = useState(0);
  const options = matchVariables(variables, value === variables.find((v) => v.path === value)?.path ? "" : value).slice(0, 30);
  // Closes on a press or focus outside, Escape or a pick; never on a blur alone (Safari).
  const wrap = useRef<HTMLSpanElement>(null);
  useDismiss(open, [wrap], () => setOpen(false));
  return (
    <span ref={wrap} className="relative min-w-40 flex-[1.4]">
      <input
        aria-label="Field"
        role="combobox"
        aria-expanded={open && options.length > 0}
        aria-autocomplete="list"
        value={value}
        spellCheck={false}
        onFocus={() => setOpen(true)}
        onChange={(e) => (onChange(e.target.value), setOpen(true), setIndex(0))}
        onKeyDown={(e) => {
          if (!open || !options.length) return;
          if (e.key === "ArrowDown" || e.key === "ArrowUp") {
            e.preventDefault();
            setIndex((i) => (i + (e.key === "ArrowDown" ? 1 : options.length - 1)) % options.length);
          } else if (e.key === "Enter") {
            e.preventDefault();
            onChange(options[index].path);
            setOpen(false);
          } else if (e.key === "Escape") {
            e.preventDefault();
            e.stopPropagation();
            setOpen(false);
          }
        }}
        placeholder="event.data.…"
        className={cn(fieldClass, "w-full font-mono text-[12px]", invalid && "border-danger/50")}
      />
      <Pop open={open && options.length > 0} as="ul" role="listbox" aria-label="Fields" className="absolute inset-x-0 top-full z-[140] mt-1 max-h-56 min-w-64 overflow-y-auto rounded-[10px] border border-outline bg-panel-2 p-1 shadow-pop inset-shadow-highlight" onMouseDown={(e) => e.preventDefault()}>
        {options.map((v, i) => (
          <li key={v.path} role="option" aria-selected={i === index} onMouseEnter={() => setIndex(i)} onClick={() => (onChange(v.path), setOpen(false))} className={cn("flex min-h-7 cursor-pointer items-center gap-2 rounded-md px-2 font-mono text-[12px]", i === index && "bg-fg/[0.08]")}>
            <span className="truncate">{v.path}</span>
            <span className="ml-auto shrink-0 font-sans text-[11px] text-subtle">{v.group}</span>
          </li>
        ))}
      </Pop>
    </span>
  );
}

function FiltersCard({ filters, onChange, variables, problems, verdicts, sample }: { filters: Filter[]; onChange: (f: Filter[]) => void; variables: readonly Variable[]; problems: Problems; verdicts: boolean[] | null; sample: (p: string) => string | undefined }) {
  const bad = problemsIn(problems, "filters").length > 0;
  const allPass = verdicts ? verdicts.every(Boolean) : null;
  return (
    <FlowCard label="Conditions" tone={bad ? "danger" : undefined}>
      <CardHead kicker="Only if" title={filters.length ? `${filters.length} condition${filters.length === 1 ? "" : "s"}` : "Every event"} icon={<Tile icon={FilterIcon} />}>
        {allPass !== null && filters.length > 0 && <Verdict ok={allPass} label={allPass ? "All pass" : "Won't run"} />}
      </CardHead>
      <div className="border-t border-line px-3.5 py-3">
        {!filters.length && <p className="mb-2 text-[12px] text-subtle">Nothing filters the events: each one starts a run.</p>}
        <FilterRows filters={filters} onChange={onChange} variables={variables} problems={problems} at="filters" verdicts={verdicts} sample={sample} />
      </div>
    </FlowCard>
  );
}

// ── a step ──

function StepCard({
  step: s,
  index,
  count,
  draft,
  catalog,
  problems,
  open,
  onToggle,
  onChange,
  onMove,
  onRemove,
  onDuplicate,
  secrets,
  sample,
  tested,
}: {
  step: Step;
  index: number;
  count: number;
  draft: AutomationSpec;
  catalog: Catalog;
  problems: Problems;
  open: boolean;
  onToggle: () => void;
  onChange: (s: Step) => void;
  onMove: (by: number) => void;
  onRemove: () => void;
  onDuplicate: () => void;
  secrets: readonly string[];
  sample: (p: string) => string | undefined;
  tested: TestResult["steps"][number] | null;
}) {
  const info = stepInfo(catalog, s.kind);
  const schema = schemaOf(info);
  const variables = useMemo(() => variablesAt(draft, catalog, index), [draft, catalog, index]);
  const bad = problemsIn(problems, `steps.${index}`);
  const head = problems[`steps.${index}`];
  const tried = tested?.runs && tested.config && typeof tested.config === "object" ? (tested.config as Record<string, unknown>) : null;
  // Once tested, the line reads what its templates made.
  const summary = useStepSummary(tried ? { ...s, config: tried } : s, schema);
  const title = s.name || info?.title || s.kind;
  const config = (s.config ?? {}) as Record<string, unknown>;
  const preview = tested?.runs ? (tested.config as Record<string, unknown>) : null;
  return (
    <FlowCard label={`Step ${index + 1}: ${title}`} tone={bad.length ? "danger" : tested ? (tested.error ? "danger" : tested.runs ? undefined : "muted") : undefined}>
      <CardHead kicker={`Step ${index + 1}`} title={title} icon={<Tile icon={stepIcon(s.kind)} />} onClick={onToggle} open={open}>
        {tested && (tested.error ? <Verdict ok={false} label="Error" /> : <Verdict ok={tested.runs} label={tested.runs ? "Runs" : "Skipped"} />)}
        {bad.length > 0 && !open && <span className="text-[11.5px] text-danger">{bad.length} to fix</span>}
        <code className="hidden rounded-[4px] border border-line-2 px-1 font-mono text-[10.5px] leading-[16px] text-subtle sm:inline" title="Later steps read its output as steps.<id>.…">
          {s.id}
        </code>
        <Menu>
          <MenuTrigger aria-label={`More for step ${index + 1}`} className="grid size-7 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg data-popup-open:bg-fg/[0.06]">
            <MoreHorizontal className="size-4" />
          </MenuTrigger>
          <MenuPopup align="end" className="w-48">
            <MenuItem disabled={index === 0} onClick={() => onMove(-1)}>
              <ArrowUp /> Move up
            </MenuItem>
            <MenuItem disabled={index === count - 1} onClick={() => onMove(1)}>
              <ArrowDown /> Move down
            </MenuItem>
            <MenuItem onClick={onDuplicate}>
              <Copy /> Duplicate
            </MenuItem>
            <MenuSeparator />
            <MenuItem variant="destructive" onClick={onRemove}>
              <Trash2 /> Remove
            </MenuItem>
          </MenuPopup>
        </Menu>
        <button type="button" onClick={onToggle} aria-label={open ? "Close the step" : "Open the step"} className="grid size-7 place-items-center rounded-md text-subtle transition-colors hover:bg-fg/[0.06] hover:text-fg">
          <ChevronDown className={cn("size-4 transition-transform duration-200 motion-reduce:transition-none", open && "rotate-180")} />
        </button>
      </CardHead>
      {!open && (summary || head) && (
        <button type="button" onClick={onToggle} className="-mt-1.5 block w-full truncate px-3.5 pb-3 pl-[52px] text-left text-[12px] text-muted">
          {head ? <span className="text-danger">{head}</span> : summary}
        </button>
      )}
      {open && (
        <div className="rise space-y-4 border-t border-line px-3.5 py-3.5">
          {info?.needs_setup && (
            <p className="flex items-center gap-2 rounded-lg border border-line-2 bg-fg/[0.02] px-3 py-2 text-[12px] text-muted">
              <TriangleAlert className="size-3.5 shrink-0 text-subtle" />
              <span>
                Connect {info.integration ?? "the integration"} in{" "}
                <button type="button" onClick={() => openSettings("owner", info.integration ?? "")} className="text-fg underline-offset-2 hover:underline">
                  Agents settings
                </button>{" "}
                before this step can run.
              </span>
            </p>
          )}
          {head && <p className="text-[11.5px] text-danger">{head}</p>}
          {tested?.error && <p className="text-[11.5px] text-danger">{tested.error}</p>}
          <ConfigForm schema={schema} value={config} onChange={(next) => onChange({ ...s, config: next })} ctx={{ variables, problems, at: `steps.${index}.config`, sample, preview, secrets }} />
          <Advanced step={s} onChange={onChange} variables={variables} problems={problems} index={index} sample={sample} />
        </div>
      )}
    </FlowCard>
  );
}

export function useStepSummary(s: Step, schema: Schema): string {
  const { client } = useAgents();
  const c = (s.config ?? {}) as Record<string, unknown>;
  const parts: string[] = [];
  const target = c.target as { type: string; machine?: string; size?: string; keep?: boolean } | null | undefined;
  if (target) {
    if (target.type === "machine") parts.push(`on ${client.data.machines.get(target.machine ?? "")?.value.name ?? "a machine"}`);
    else if (target.type === "sandbox") parts.push(`in a ${target.size ?? "new"} sandbox${target.keep ? " (kept)" : ""}`);
    else if (target.type === "any_online") parts.push("on any online machine");
    else if (target.type === "project_machine") parts.push("on the project's machine");
  }
  if (typeof c.model === "string") {
    // The model's name as the machines' providers say it ("Opus 5.5"), else its id.
    let name: string | undefined;
    for (const r of client.data.machines.values()) name ??= r.value.info?.providers.flatMap((p) => p.models).find((m) => m.id === c.model)?.name;
    parts.push(name ?? c.model);
  }
  if (c.worktree === true) parts.push("worktree");
  if (c.land === true) parts.push("queued to land");
  if (Array.isArray(c.until) && c.until.length) parts.push(`until ${c.until.join(" or ")}`);
  if (typeof c.seconds === "number") parts.push(`${c.seconds}s`);
  for (const [k, p] of Object.entries(schema.properties ?? {})) {
    if ((p.format === "template" || p.format === "template-long") && typeof c[k] === "string" && c[k]) {
      parts.push(`“${(c[k] as string).replace(/\s+/g, " ").slice(0, 80)}”`);
      break;
    }
  }
  if (typeof c.url === "string" && c.url) parts.push(c.url);
  return parts.join(" · ");
}

function Advanced({ step: s, onChange, variables, problems, index, sample }: { step: Step; onChange: (s: Step) => void; variables: readonly Variable[]; problems: Problems; index: number; sample: (p: string) => string | undefined }) {
  const [open, setOpen] = useState(s.when.length > 0 || !!s.retry || s.timeout_s !== null || s.on_error === "continue" || !!problems[`steps.${index}.id`]);
  const idErr = problems[`steps.${index}.id`];
  // The wait between tries outlives one try (typing 10 passes through 1, which has no retry).
  const [backoff, setBackoff] = useState(s.retry?.backoff_s ?? 30);
  return (
    <div className="rounded-lg border border-line">
      <button type="button" onClick={() => setOpen(!open)} aria-expanded={open} className="flex h-9 w-full items-center gap-2 px-3 text-left text-[12.5px] text-muted hover:text-fg">
        <ChevronDown className={cn("size-3.5 -rotate-90 transition-transform duration-200 motion-reduce:transition-none", open && "rotate-0")} />
        Name, conditions, retries
        {s.when.length > 0 && <span className="ml-auto text-[11px] text-subtle">{s.when.length} condition{s.when.length === 1 ? "" : "s"}</span>}
      </button>
      {open && (
        <div className="space-y-3.5 border-t border-line px-3 py-3">
          <div className="flex flex-wrap gap-3">
            <label className="min-w-40 flex-1">
              <span className="mb-1 block text-[12px] font-[510] text-soft">Name</span>
              <input value={s.name ?? ""} onChange={(e) => onChange({ ...s, name: e.target.value || undefined })} placeholder="Optional" aria-label="Step name" className={cn(fieldClass, "w-full")} />
            </label>
            <label className="w-40">
              <span className="mb-1 block text-[12px] font-[510] text-soft">Id</span>
              <input value={s.id} onChange={(e) => onChange({ ...s, id: e.target.value.toLowerCase() })} aria-label="Step id" className={cn(fieldClass, "w-full font-mono text-[12px]", idErr && "border-danger/50")} />
            </label>
          </div>
          {idErr && <p className="-mt-2 text-[11.5px] text-danger">{idErr}</p>}
          <div>
            <p className="mb-1.5 text-[12px] font-[510] text-soft">Run this step only if</p>
            <FilterRows filters={s.when} onChange={(when) => onChange({ ...s, when })} variables={variables} problems={problems} at={`steps.${index}.when`} verdicts={null} sample={sample} />
          </div>
          <div className="flex flex-wrap items-end gap-4">
            <div>
              <p className="mb-1.5 text-[12px] font-[510] text-soft">If it fails</p>
              <Segmented
                label="If it fails"
                value={s.on_error ?? "fail"}
                onChange={(v) => onChange({ ...s, on_error: v === "fail" ? undefined : v })}
                options={[
                  { value: "fail", label: "Stop the run" },
                  { value: "continue", label: "Go on" },
                ]}
              />
            </div>
            <label>
              <span className="mb-1.5 block text-[12px] font-[510] text-soft">Tries</span>
              <NumberField min={1} max={10} aria-label="Tries" value={s.retry?.attempts ?? 1} onChange={(n) => n !== null && onChange({ ...s, retry: n <= 1 ? null : { attempts: n, backoff_s: s.retry?.backoff_s ?? backoff } })} className={cn(fieldClass, "w-16 font-mono text-[12.5px]")} />
            </label>
            {s.retry && (
              <label>
                <span className="mb-1.5 block text-[12px] font-[510] text-soft">Wait between (s)</span>
                <NumberField min={1} aria-label="Backoff seconds" value={s.retry.backoff_s} onChange={(n) => {
                  if (n === null) return;
                  setBackoff(n);
                  onChange({ ...s, retry: { ...s.retry!, backoff_s: n } });
                }} className={cn(fieldClass, "w-20 font-mono text-[12.5px]")} />
              </label>
            )}
            <label>
              <span className="mb-1.5 block text-[12px] font-[510] text-soft">Time limit (s)</span>
              <NumberField optional min={1} aria-label="Time limit seconds" value={s.timeout_s ?? null} placeholder="Default" onChange={(n) => onChange({ ...s, timeout_s: n })} className={cn(fieldClass, "w-24 font-mono text-[12.5px]")} />
            </label>
          </div>
        </div>
      )}
    </div>
  );
}

let stepKeys = 0;

/**
 * Keys for the step cards that follow a step, not its id: typing in its id field renames it in
 * place (the card, and its open "Advanced", stay), moving it carries its key along, a new one gets
 * a new key.
 */
function useStepKeys(ids: string[]): string[] {
  const [memo, setMemo] = useState(() => ({ ids, keys: ids.map(() => `step${stepKeys++}`) }));
  if (memo.ids.length === ids.length && memo.ids.every((x, i) => x === ids[i])) return memo.keys;
  const renamed = memo.ids.length === ids.length;
  const keys = ids.map((id, i) => {
    const j = memo.ids.indexOf(id);
    if (j >= 0) return memo.keys[j];
    if (renamed && !ids.includes(memo.ids[i])) return memo.keys[i];
    return `step${stepKeys++}`;
  });
  setMemo({ ids, keys });
  return keys;
}
