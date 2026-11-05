"use client";

/**
 * The automations list in the sheet (docs/code/automations-ui.md, "The list"): dense two-line rows,
 * Linear's issue rows in spirit. Each row has:
 * - the trigger's tile, with a dot that breathes while a run goes on
 * - the name, where it works, and the marks of the integrations its steps use
 * - the trigger in words with the next run
 * - its last run (state and how long ago) and its last runs as bars
 * - the switch
 * On hover or focus, Run now, Pause, Duplicate, Edit and ⋯ take the last run's place; on touch, ⋯
 * has them all. Above the rows: search, and Status, Trigger and Project filters shown as removable
 * chips. ↑/↓ move between rows. The logic is lib/automations-list.ts.
 */

import { Check, Copy, ListFilter, MoreHorizontal, Pause, Pencil, Play, Trash2, X } from "lucide-react";
import { useMemo, useState, type KeyboardEvent, type ReactNode } from "react";
import type { Automation, AutomationRun } from "@/lib/automations-api";
import { setAgentsView, type AutomationsView as View } from "@/lib/agents-surface";
import { filterAutomations, filterOptions, liveRunOf, recentRunsOf, type ListFilter as Filter, type StatusFilter } from "@/lib/automations-list";
import { lastRunOf, runLabel, triggerSource, triggerSummary } from "@/lib/automations-ui";
import { automationPlaces } from "@/lib/agents-identity";
import { integrationOf } from "../brand-icons";
import { ProjectIcon } from "../identity";
import { Menu } from "../menu";
import { ago } from "../shared";
import { cn, Popover, PopoverPopup, PopoverTrigger, Switch } from "../ui";
import { useAgents } from "../use-agents";
import { useAutomationActions } from "./actions";
import { Places } from "./automations-view";
import { inTime, RunDot, shortAgo, Tile, triggerIcon, useAutomations } from "./shared";

const TRIGGER_WORDS: Record<string, string> = { manual: "By hand", schedule: "Schedule", interval: "Interval", webhook: "Webhook", code: "roll code", agent: "Agents" };

export const triggerLabel = (source: string) => TRIGGER_WORDS[source] ?? integrationOf(source)?.name ?? source[0].toUpperCase() + source.slice(1);

const STATUS_WORDS: Record<StatusFilter, string> = { on: "On", paused: "Paused", failing: "Last run failed", running: "Running now" };

/** The list with its search and filters; `query` is the sheet's search field. */
export function AutomationList({ automations, query, setQuery, filter, setFilter, list }: { automations: Automation[]; query: string; setQuery: (q: string) => void; filter: Filter; setFilter: (f: Filter) => void; list?: View["list"] }) {
  const { runs, catalog } = useAutomations();
  const { client } = useAgents();
  const data = client.data;
  const [now] = useState(() => Date.now());
  const projectsOf = useProjectsOf();
  const ctx = useMemo(() => ({ runs: [...runs.values()], summary: (a: Automation) => triggerSummary(a.trigger, catalog), projectsOf }), [runs, catalog, projectsOf]);
  const shown = useMemo(() => filterAutomations(automations, query, filter, ctx), [automations, query, filter, ctx]);
  const active = Object.entries(filter).filter(([, v]) => v !== undefined) as [keyof Filter, string][];
  const chipLabel = (k: keyof Filter, v: string) => (k === "status" ? STATUS_WORDS[v as StatusFilter] : k === "trigger" ? triggerLabel(v) : (data.projects.get(v)?.value.name ?? v));
  return (
    <>
      {active.length > 0 && (
        <div className="mb-2 flex flex-wrap items-center gap-1.5" aria-label="Filters">
          {active.map(([k, v]) => (
            <span key={k} className="inline-flex h-6 items-center gap-1 rounded-md border border-line-2 bg-fg/[0.04] pr-0.5 pl-2 text-[12px] text-soft">
              <span className="text-subtle">{k === "status" ? "Status" : k === "trigger" ? "Trigger" : "Project"}</span> {chipLabel(k, v)}
              <button type="button" onClick={() => setFilter({ ...filter, [k]: undefined })} aria-label={`Remove the filter ${chipLabel(k, v)}`} className="grid size-5 place-items-center rounded text-subtle hover:bg-fg/[0.06] hover:text-fg">
                <X className="size-3" />
              </button>
            </span>
          ))}
          <button type="button" onClick={() => setFilter({})} className="ml-1 text-[12px] text-subtle hover:text-fg">
            Clear
          </button>
        </div>
      )}
      {shown.length ? (
        <ul className="overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight" aria-label="Your automations" onKeyDown={moveBetweenRows}>
          {shown.map((x) => (
            <Row key={x.id} automation={x} runs={runs} now={now} list={list} />
          ))}
        </ul>
      ) : (
        <p className="py-12 text-center text-[13px] text-subtle">
          Nothing matches{query.trim() ? ` “${query.trim()}”` : " these filters"}.{" "}
          <button
            onClick={() => {
              setQuery("");
              setFilter({});
            }}
            className="text-accent hover:underline"
          >
            Clear {query.trim() && active.length ? "both" : query.trim() ? "the search" : "the filters"}
          </button>
        </p>
      )}
    </>
  );
}

/** ↑/↓ from one row to the next (Linear's list keys). */
function moveBetweenRows(e: KeyboardEvent<HTMLUListElement>) {
  if (e.key !== "ArrowDown" && e.key !== "ArrowUp") return;
  const rows = [...e.currentTarget.querySelectorAll<HTMLElement>("[data-row]")];
  const at = rows.findIndex((r) => r === document.activeElement || r.contains(document.activeElement));
  const next = rows[at < 0 ? 0 : Math.max(0, Math.min(rows.length - 1, at + (e.key === "ArrowDown" ? 1 : -1)))];
  if (!next) return;
  e.preventDefault();
  next.focus();
}

/** Project ids each automation works in, as the canvas shows them (`automationPlaces`). */
function useProjectsOf(): (a: Automation) => readonly string[] {
  const { client } = useAgents();
  const data = client.data;
  return useMemo(() => {
    const cache = new WeakMap<Automation, string[]>();
    const projects = [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine }));
    return (a: Automation) => {
      let ids = cache.get(a);
      if (!ids) {
        ids = automationPlaces(a, projects, []).projects.map((p) => p.id);
        cache.set(a, ids);
      }
      return ids;
    };
  }, [data.projects]);
}

/** The Filter button: Status, Trigger and Project, one value each, with counts. */
export function FilterButton({ automations, filter, setFilter }: { automations: Automation[]; filter: Filter; setFilter: (f: Filter) => void }) {
  const { runs } = useAutomations();
  const { client } = useAgents();
  const projectsOf = useProjectsOf();
  const [open, setOpen] = useState(false);
  const options = useMemo(() => filterOptions(automations, { runs: [...runs.values()], projectsOf }), [automations, runs, projectsOf]);
  const on = Object.values(filter).filter(Boolean).length;
  const pick = (k: keyof Filter, v: string) => setFilter({ ...filter, [k]: filter[k] === v ? undefined : v });
  return (
    <Popover open={open} onOpenChange={setOpen}>
      <PopoverTrigger
        render={
          <button
            type="button"
            aria-label={on ? `Filter (${on} on)` : "Filter"}
            title="Filter"
            className={cn("inline-flex h-7 shrink-0 items-center max-sm:ml-auto gap-1.5 rounded-md px-2 text-[12.5px] transition-colors hover:bg-fg/[0.06] hover:text-fg", on ? "text-fg" : "text-muted")}
          />
        }
      >
        <ListFilter className="size-3.5" strokeWidth={1.75} />
        <span className="max-sm:sr-only">Filter</span>
        {on > 0 && <span className="grid h-4 min-w-4 place-items-center rounded-full bg-fg/[0.1] px-1 font-mono text-[10px] tabular-nums">{on}</span>}
      </PopoverTrigger>
      <PopoverPopup side="bottom" align="end" sideOffset={6} aria-label="Filter automations" className="w-60 flex-col overflow-y-auto p-1">
        <Group title="Status">
          {options.status.map((o) => (
            <Option key={o.value} on={filter.status === o.value} count={o.count} onClick={() => pick("status", o.value)} disabled={!o.count}>
              {STATUS_WORDS[o.value]}
            </Option>
          ))}
        </Group>
        {options.trigger.length > 1 && (
          <Group title="Trigger">
            {options.trigger.map((o) => {
              const Icon = triggerIcon(o.value);
              return (
                <Option key={o.value} on={filter.trigger === o.value} count={o.count} onClick={() => pick("trigger", o.value)} icon={<Icon className="size-3.5 text-subtle" strokeWidth={1.75} />}>
                  {triggerLabel(o.value)}
                </Option>
              );
            })}
          </Group>
        )}
        {options.project.length > 0 && (
          <Group title="Project">
            {options.project.map((o) => {
              const p = client.data.projects.get(o.value)?.value;
              return (
                <Option key={o.value} on={filter.project === o.value} count={o.count} onClick={() => pick("project", o.value)} icon={p && <ProjectIcon project={p} className="size-3.5" />}>
                  {p?.name ?? o.value}
                </Option>
              );
            })}
          </Group>
        )}
      </PopoverPopup>
    </Popover>
  );
}

function Group({ title, children }: { title: string; children: ReactNode }) {
  return (
    <div role="group" aria-label={title} className="py-1 [&+&]:border-t [&+&]:border-line">
      <p className="px-2 pt-0.5 pb-1 text-[11px] font-medium text-subtle">{title}</p>
      {children}
    </div>
  );
}

function Option({ on, count, onClick, disabled, icon, children }: { on: boolean; count: number; onClick: () => void; disabled?: boolean; icon?: ReactNode; children: ReactNode }) {
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-pressed={on}
      className="flex h-7 w-full items-center gap-2 rounded-md px-2 text-left text-[12.5px] text-soft transition-colors hover:bg-fg/[0.06] hover:text-fg disabled:pointer-events-none disabled:opacity-40"
    >
      {icon}
      <span className="min-w-0 flex-1 truncate">{children}</span>
      <span className="font-mono text-[10.5px] text-subtle tabular-nums">{count}</span>
      <Check className={cn("size-3.5 text-fg", !on && "invisible")} />
    </button>
  );
}

/** The integrations its steps reach (Slack, Linear, GitHub…) besides its trigger, as small marks. */
function StepMarks({ automation: x }: { automation: Automation }) {
  const trigger = triggerSource(x.trigger);
  const marks = [...new Map(x.steps.map((s) => integrationOf(s.kind)).filter((i): i is NonNullable<typeof i> => !!i && i !== integrationOf(trigger)).map((i) => [i.name, i])).values()];
  if (!marks.length) return null;
  return (
    <span role="img" aria-label={`Uses ${marks.map((m) => m.name).join(", ")}`} title={marks.map((m) => m.name).join(" · ")} className="flex shrink-0 items-center gap-1 text-subtle">
      {marks.map((m) => (
        <m.Mark key={m.name} className="size-3.5" />
      ))}
    </span>
  );
}

const BARS = 12;
const barTone: Record<AutomationRun["status"], string> = {
  succeeded: "bg-fg/45",
  failed: "bg-danger/85",
  running: "bg-accent pulse-dot",
  waiting: "bg-accent pulse-dot",
  queued: "bg-accent/50",
  cancelled: "bg-fg/15",
  skipped: "bg-fg/15",
};

/** Its last runs as bars, oldest first (GitHub Actions' run history); empty slots keep rows aligned. */
export function RunBars({ runs, now }: { runs: AutomationRun[]; now: number }) {
  const failed = runs.filter((r) => r.status === "failed").length;
  return (
    <span className="flex h-3.5 items-stretch gap-[2px]" role="img" aria-label={runs.length ? `Last ${runs.length} run${runs.length === 1 ? "" : "s"}${failed ? `, ${failed} failed` : ", none failed"}` : "No runs yet"}>
      {Array.from({ length: BARS - runs.length }, (_, i) => (
        <span key={`e${i}`} className="w-[3px] rounded-[1px] bg-fg/[0.06]" />
      ))}
      {runs.map((r) => (
        <span key={r.id} className={cn("w-[3px] rounded-[1px]", barTone[r.status])} title={`${runLabel[r.status]} · ${ago(r.created_at, now)}`} />
      ))}
    </span>
  );
}

function Row({ automation: x, runs, now, list }: { automation: Automation; runs: ReadonlyMap<string, AutomationRun>; now: number; list?: View["list"] }) {
  const { catalog } = useAutomations();
  const actions = useAutomationActions(x, list);
  const last = lastRunOf(x, runs.values());
  const live = liveRunOf(x.id, runs.values());
  const recent = useMemo(() => recentRunsOf(x.id, runs.values(), BARS), [runs, x.id]);
  const open = () => setAgentsView({ kind: "automations", list, open: x.id });
  const edit = () => setAgentsView({ kind: "automations", list, open: x.id, tab: "edit" });
  const when = !x.enabled ? "Paused" : [triggerSummary(x.trigger, catalog), x.next_at && x.next_at > now ? `next ${inTime(x.next_at, now)}` : null].filter(Boolean).join(" · ");
  const more = [
    { label: "Run now", icon: <Play />, onSelect: () => void actions.runNow() },
    { label: x.enabled ? "Pause" : "Turn on", icon: x.enabled ? <Pause /> : <Play />, onSelect: () => void actions.setEnabled(!x.enabled) },
    { label: "Duplicate", icon: <Copy />, onSelect: () => void actions.duplicate() },
    { label: "Edit", icon: <Pencil />, onSelect: edit },
    { label: "Delete", icon: <Trash2 />, danger: true, confirm: `Delete ${x.name}?`, onSelect: () => void actions.remove() },
  ];
  return (
    <li className="border-line [&+&]:border-t">
      <div
        role="button"
        tabIndex={0}
        data-row
        onClick={open}
        onKeyDown={(e) => {
          if ((e.key === "Enter" || e.key === " ") && e.target === e.currentTarget) {
            e.preventDefault();
            open();
          }
        }}
        aria-label={x.name}
        className="group/row relative flex min-h-[52px] cursor-pointer items-center gap-3 px-3 py-2 outline-none transition-colors duration-150 hover:bg-fg/[0.03] focus-visible:bg-fg/[0.05]"
      >
        <span className={cn("relative transition-opacity", !x.enabled && "opacity-60")}>
          <Tile icon={triggerIcon(triggerSource(x.trigger))} />
          {live && <RunDot status={live.status} className="absolute -top-0.5 -right-0.5 ring-2 ring-panel" />}
        </span>
        <span className={cn("min-w-0 flex-1 transition-opacity", !x.enabled && "opacity-64")}>
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate text-[13.5px] font-[510] tracking-[-0.01em]">{x.name}</span>
            <span className="hidden items-center gap-2 @md:flex">
              <Places automation={x} />
              <StepMarks automation={x} />
            </span>
          </span>
          <span className="mt-px block truncate text-[12px] text-subtle">
            {when}
            {actions.error && <span className="ml-2 text-danger">{actions.error}</span>}
          </span>
        </span>
        {/* The last run and its history; hover and focus put the actions in the same place. */}
        {/* Narrow lists and touch keep only ⋯ (beside the switch), so the row's text keeps its room. */}
        <span className="grid shrink-0 items-center justify-items-end">
          <span className="flex items-center gap-3 [grid-area:1/1] @md:group-focus-within/row:invisible @md:group-hover/row:invisible [@media(hover:none)]:visible!">
            <LastRun live={live} last={last} now={now} />
            <span className="hidden @md:flex">
              <RunBars runs={recent} now={now} />
            </span>
          </span>
          <span
            className="invisible hidden items-center gap-0.5 text-muted [grid-area:1/1] group-focus-within/row:visible group-hover/row:visible @md:flex [@media(hover:none)]:hidden!"
            aria-label={`Actions for ${x.name}`}
            role="group"
            onClick={(e) => e.stopPropagation()}
            onKeyDown={(e) => e.stopPropagation()}
          >
            <RowAction label="Run now" onClick={() => void actions.runNow()} disabled={actions.busy === "run"}>
              <Play />
            </RowAction>
            <RowAction label={x.enabled ? "Pause" : "Turn on"} onClick={() => void actions.setEnabled(!x.enabled)} disabled={actions.busy === "toggle"}>
              {x.enabled ? <Pause /> : <Play />}
            </RowAction>
            <RowAction label="Duplicate" onClick={() => void actions.duplicate()} disabled={actions.busy === "duplicate"}>
              <Copy />
            </RowAction>
            <RowAction label="Edit" onClick={edit}>
              <Pencil />
            </RowAction>
            <Menu label={`More for ${x.name}`} trigger={<MoreHorizontal className="size-4" />} items={more.slice(4)} />
          </span>
        </span>
        <span className="flex shrink-0 items-center gap-1" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Switch size="sm" checked={x.enabled} disabled={actions.busy === "toggle"} onCheckedChange={(on) => void actions.setEnabled(on)} aria-label={`${x.enabled ? "Pause" : "Turn on"} ${x.name}`} />
          <span className="inline-flex @md:hidden [@media(hover:none)]:inline-flex!">
            <Menu label={`Actions for ${x.name}`} trigger={<MoreHorizontal className="size-4" />} items={more} />
          </span>
        </span>
      </div>
    </li>
  );
}

function LastRun({ live, last, now }: { live: AutomationRun | null; last: ReturnType<typeof lastRunOf>; now: number }) {
  if (live)
    return (
      <span className="flex items-center gap-1.5 text-[12px] text-accent">
        <RunDot status={live.status} />
        <span className="shimmer max-sm:sr-only">{runLabel[live.status]}</span>
      </span>
    );
  if (!last) return <span className="text-[12px] text-faint">Never run</span>;
  return (
    <span className="flex items-center gap-1.5 text-[12px] text-muted" title={`${runLabel[last.status]} ${ago(last.at, now)}`}>
      <RunDot status={last.status} />
      <span className={cn("hidden @lg:inline", last.status === "failed" && "text-danger")}>{runLabel[last.status]}</span>
      <span className="font-mono text-[11px] text-subtle tabular-nums">{shortAgo(last.at, now)}</span>
    </span>
  );
}

function RowAction({ label, onClick, disabled, children }: { label: string; onClick: () => void; disabled?: boolean; children: ReactNode }) {
  return (
    <button type="button" onClick={onClick} disabled={disabled} aria-label={label} title={label} className="grid size-7 place-items-center rounded-md transition-colors hover:bg-fg/[0.07] hover:text-fg disabled:opacity-40 [&_svg]:size-3.5 [&_svg]:stroke-[1.75]">
      {children}
    </button>
  );
}

