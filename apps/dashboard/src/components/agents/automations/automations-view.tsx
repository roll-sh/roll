"use client";

/**
 * The Automations view: in place of the canvas, inside the same inset panel (docs/code/
 * automations-ui.md). Three lists under one bar: the automations (two-line rows: trigger, name,
 * when it runs, its last run, seven days of runs, the switch), every run, and the templates; the
 * empty state is the templates gallery. Opening one puts it in the side panel.
 */

import { DotField } from "@roll/brand";
import { ArrowRight, ChevronRight, Copy, MoreHorizontal, Play, Plus, Search, Trash2, X } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import type { Automation, AutomationRun, RunState } from "@/lib/automations-api";
import { TEMPLATES, type AutomationTemplate, type TemplateContext } from "@/lib/automation-templates";
import { setAgentsView, type AutomationsView as View } from "@/lib/agents-surface";
import { duration, lastRunOf, runLabel, specOf, took, triggerSummary, weekOf } from "@/lib/automations-ui";
import { automationPlaces } from "@/lib/agents-identity";
import { MachineIcon, ProjectIcon } from "../identity";
import { Menu } from "../menu";
import { integrationOf } from "../brand-icons";
import { ago } from "../shared";
import { cn, Segmented, Switch } from "../ui";
import { useAgents } from "../use-agents";
import { inTime, PrimaryButton, QuietButton, RunDot, RunStatus, shortAgo, stepIcon, Tile, triggerIcon, useAutomations, WeekStrip } from "./shared";

export function AutomationsView({ view, phone }: { view: View; phone: boolean }) {
  const a = useAutomations();
  const list = view.list ?? "automations";
  const go = (patch: Partial<View>) => setAgentsView({ kind: "automations", ...patch });
  const automations = useMemo(() => [...a.automations.values()].sort((x, y) => Number(y.enabled) - Number(x.enabled) || x.name.localeCompare(y.name)), [a.automations]);
  const [query, setQuery] = useState("");
  const shown = query.trim() ? automations.filter((x) => `${x.name} ${x.description ?? ""} ${triggerSummary(x.trigger, a.catalog)}`.toLowerCase().includes(query.trim().toLowerCase())) : automations;
  const empty = a.loaded && !a.error && automations.length === 0;

  if (!a.store) return <Center title="Automations aren't on this server yet" body="Update roll to set up triggers that run your agents." />;
  return (
    <div className="flex h-full min-h-0 flex-col @container" aria-label="Automations">
      <div className="flex h-11 shrink-0 items-center gap-2 px-3 max-sm:px-2">
        <Segmented
          label="Show"
          value={list}
          onChange={(v) => go({ list: v === "automations" ? undefined : v })}
          options={[
            { value: "automations", label: <>Automations{automations.length > 0 && <Count n={automations.length} />}</> },
            { value: "runs", label: <>Runs{running(a.runs) > 0 && <Count n={running(a.runs)} accent />}</> },
            { value: "templates", label: "Templates" },
          ]}
        />
        <span className="ml-auto flex items-center gap-1.5">
          {list === "automations" && automations.length > 0 && !phone && (
            <label className="flex h-7 w-52 items-center gap-2 rounded-md border border-line bg-fg/[0.03] px-2 text-[12.5px] inset-shadow-highlight transition-colors focus-within:border-line-3 max-lg:w-40">
              <Search className="size-3.5 shrink-0 text-subtle" aria-hidden />
              <input value={query} onChange={(e) => setQuery(e.target.value)} placeholder="Filter" aria-label="Filter automations" className="min-w-0 flex-1 bg-transparent outline-none placeholder:text-subtle" />
              {query && (
                <button type="button" onClick={() => setQuery("")} aria-label="Clear the filter" className="text-subtle hover:text-fg">
                  <X className="size-3" />
                </button>
              )}
            </label>
          )}
          <PrimaryButton onClick={() => go({ open: "new", list: view.list })} title="New automation">
            <Plus strokeWidth={2} />
            <span className="max-sm:sr-only">New automation</span>
          </PrimaryButton>
        </span>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto" data-scroll>
        {!a.loaded ? (
          <Loading />
        ) : a.error && !automations.length ? (
          <Center title="Couldn't load your automations" body={a.error} action={<QuietButton onClick={() => void a.store?.refresh()}>Try again</QuietButton>} />
        ) : list === "templates" ? (
          <Section>
            <Gallery open={view.template} />
          </Section>
        ) : list === "runs" ? (
          <Section>
            <AllRuns runs={a.runs} automations={a.automations} selected={view.run} />
          </Section>
        ) : empty ? (
          <EmptyState />
        ) : (
          <Section>
            {shown.length ? (
              <List automations={shown} runs={a.runs} selected={view.open} />
            ) : (
              <p className="py-16 text-center text-[13px] text-subtle">
                Nothing matches “{query}”.{" "}
                <button onClick={() => setQuery("")} className="text-accent hover:underline">
                  Clear the filter
                </button>
              </p>
            )}
            <p className="mt-4 px-1 text-[12px] text-subtle">
              Automations run your agents when something happens.{" "}
              <button onClick={() => go({ list: "templates" })} className="text-accent hover:underline">
                Browse templates
              </button>
            </p>
          </Section>
        )}
      </div>
    </div>
  );
}

const running = (runs: ReadonlyMap<string, AutomationRun>) => [...runs.values()].filter((r) => r.status === "running" || r.status === "waiting" || r.status === "queued").length;

function Count({ n, accent }: { n: number; accent?: boolean }) {
  return <span className={cn("ml-1 grid h-4 min-w-4 place-items-center rounded-full px-1 font-mono text-[10px] tabular-nums", accent ? "bg-accent/20 text-accent" : "bg-fg/[0.08] text-muted")}>{n}</span>;
}

function Section({ children }: { children: ReactNode }) {
  return <div className="mx-auto w-full max-w-[1040px] px-5 pt-3 pb-10 max-sm:px-2">{children}</div>;
}

function Loading() {
  return (
    <Section>
      <div className="overflow-hidden rounded-xl border border-line-2 bg-panel" aria-busy="true" aria-label="Loading automations">
        {[0, 1, 2].map((i) => (
          <div key={i} className="flex h-[52px] items-center gap-3 border-line px-3.5 [&+&]:border-t">
            <span className="size-7 rounded-lg bg-panel-2" />
            <span className="h-3 w-48 rounded bg-panel-2 shimmer" />
          </div>
        ))}
      </div>
    </Section>
  );
}

function Center({ title, body, action }: { title: string; body: string; action?: ReactNode }) {
  return (
    <div className="fade-in grid h-full place-items-center px-6 text-center">
      <div className="space-y-2">
        <p className="text-[13.5px] font-medium tracking-[-0.01em]">{title}</p>
        <p className="text-[12.5px] text-subtle">{body}</p>
        {action && <div className="pt-2">{action}</div>}
      </div>
    </div>
  );
}

// ── the list ──

function List({ automations, runs, selected }: { automations: Automation[]; runs: ReadonlyMap<string, AutomationRun>; selected?: string }) {
  const [now] = useState(() => Date.now());
  return (
    <ul className="overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight" aria-label="Your automations">
      {automations.map((x) => (
        <Row key={x.id} automation={x} runs={runs} selected={x.id === selected} now={now} />
      ))}
    </ul>
  );
}

/** The projects and machines an automation works in, as the canvas shows them (a quiet stack after its name). */
function Places({ automation }: { automation: Automation }) {
  const { client } = useAgents();
  const data = client.data;
  const { projects, machines } = useMemo(
    () =>
      automationPlaces(
        automation,
        [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine })),
        [...data.machines.values()].map((r) => r.value),
      ),
    [automation, data.projects, data.machines],
  );
  if (!projects.length && !machines.length) return null;
  const names = [...projects.map((p) => p.name), ...machines.map((m) => m.name)];
  const shown = 3;
  return (
    <span role="img" aria-label={`In ${names.join(", ")}`} title={names.join(" · ")} className="flex shrink-0 items-center gap-1">
      {projects.slice(0, shown).map((p) => (
        <ProjectIcon key={p.id} project={p} className="size-3.5" />
      ))}
      {machines.slice(0, Math.max(0, shown - projects.length)).map((m) => (
        <MachineIcon key={m.id} machine={m} className="size-3.5" />
      ))}
      {names.length > shown && <span className="font-mono text-[10.5px] text-subtle tabular-nums">+{names.length - shown}</span>}
    </span>
  );
}

function Row({ automation: x, runs, selected, now }: { automation: Automation; runs: ReadonlyMap<string, AutomationRun>; selected: boolean; now: number }) {
  const { store, catalog } = useAutomations();
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const last = lastRunOf(x, runs.values());
  const week = useMemo(() => weekOf(runs.values(), x.id, now), [runs, x.id, now]);
  const source = x.trigger.type === "integration" ? x.trigger.source : x.trigger.type;
  const open = () => setAgentsView({ kind: "automations", open: x.id });
  const act = async (fn: () => Promise<unknown>) => {
    setError(null);
    try {
      await fn();
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  };
  const toggle = (on: boolean) =>
    act(async () => {
      if (!store) return;
      setBusy(true);
      try {
        store.put(on ? await store.api.enable(store.owner, x.id) : await store.api.disable(store.owner, x.id));
      } finally {
        setBusy(false);
      }
    });
  const runNow = () =>
    act(async () => {
      if (!store) return;
      const r = await store.api.run(store.owner, x.id, {});
      store.putRun(r);
      setAgentsView({ kind: "automations", open: x.id, tab: "runs", run: r.id });
    });
  const duplicate = () =>
    act(async () => {
      if (!store) return;
      const saved = await store.api.create(store.owner, { ...specOf(x), name: `${x.name} (copy)`, enabled: false });
      store.put(saved.automation);
      setAgentsView({ kind: "automations", open: saved.automation.id });
    });
  const remove = () =>
    act(async () => {
      if (!store) return;
      await store.api.remove(store.owner, x.id);
      store.forget(x.id);
    });
  const when = !x.enabled ? "Paused" : [triggerSummary(x.trigger, catalog), x.next_at && x.next_at > now ? `next ${inTime(x.next_at, now)}` : null].filter(Boolean).join(" · ");
  return (
    <li className="border-line [&+&]:border-t">
      <div
        role="button"
        tabIndex={0}
        onClick={open}
        onKeyDown={(e) => {
          if ((e.key === "Enter" || e.key === " ") && e.target === e.currentTarget) {
            e.preventDefault();
            open();
          }
        }}
        aria-label={x.name}
        aria-current={selected || undefined}
        className={cn(
          "group/row relative flex min-h-[52px] cursor-pointer items-center gap-3 px-3.5 py-2 outline-none transition-colors duration-150 hover:bg-fg/[0.03] focus-visible:bg-fg/[0.04]",
          selected && "bg-fg/[0.06] hover:bg-fg/[0.06]",
        )}
      >
        <span className={cn("relative transition-opacity", !x.enabled && "opacity-60")}>
          <Tile icon={triggerIcon(source)} />
          {last && <RunDot status={last.status} className="absolute -top-0.5 -right-0.5 ring-2 ring-panel @2xl:hidden" />}
        </span>
        <span className={cn("min-w-0 flex-1 transition-opacity", !x.enabled && "opacity-64")}>
          <span className="flex min-w-0 items-center gap-2">
            <span className="truncate text-[13.5px] font-[510] tracking-[-0.01em]">{x.name}</span>
            <Places automation={x} />
          </span>
          <span className="mt-px block truncate text-[12px] text-subtle">
            {when}
            {error && <span className="ml-2 text-danger">{error}</span>}
          </span>
        </span>
        <span className="hidden w-36 shrink-0 items-center gap-1.5 text-[12px] text-muted @2xl:flex">
          {last ? (
            <>
              <RunDot status={last.status} />
              <span className={cn("truncate", last.status === "failed" && "text-danger")}>{runLabel[last.status]}</span>
              <span className="ml-auto font-mono text-[11px] text-subtle tabular-nums">{shortAgo(last.at, now)}</span>
            </>
          ) : (
            <span className="text-faint">Never run</span>
          )}
        </span>
        <span className="hidden shrink-0 @lg:inline-flex">
          <WeekStrip {...week} />
        </span>
        <span className="flex shrink-0 items-center gap-1" onClick={(e) => e.stopPropagation()} onKeyDown={(e) => e.stopPropagation()}>
          <Switch size="sm" checked={x.enabled} disabled={busy} onCheckedChange={(on) => void toggle(on)} aria-label={`${x.enabled ? "Pause" : "Turn on"} ${x.name}`} />
          <span className="opacity-0 transition-opacity group-hover/row:opacity-100 focus-within:opacity-100 [@media(hover:none)]:opacity-100">
            <Menu
              label={`More for ${x.name}`}
              trigger={<MoreHorizontal className="size-4" />}
              items={[
                { label: "Run now", icon: <Play />, onSelect: () => void runNow() },
                { label: "Duplicate", icon: <Copy />, onSelect: () => void duplicate() },
                { label: "Delete", icon: <Trash2 />, danger: true, confirm: `Delete ${x.name}?`, onSelect: () => void remove() },
              ]}
            />
          </span>
        </span>
      </div>
    </li>
  );
}

// ── every run ──

const FILTERS: { value: "all" | RunState; label: string }[] = [
  { value: "all", label: "All" },
  { value: "failed", label: "Failed" },
  { value: "running", label: "Running" },
];

export function RunRows({ runs, selected, showName, onOpen, now }: { runs: AutomationRun[]; selected?: string; showName: boolean; onOpen: (r: AutomationRun) => void; now: number }) {
  return (
    <ul className="overflow-hidden rounded-xl border border-line-2 bg-panel shadow-card inset-shadow-highlight" aria-label="Runs">
      {runs.map((r) => {
        const ms = took(r.started_at, r.finished_at, now);
        const done = r.steps.filter((s) => s.status === "succeeded").length;
        return (
          <li key={r.id} className="border-line [&+&]:border-t">
            <button
              type="button"
              onClick={() => onOpen(r)}
              aria-current={r.id === selected || undefined}
              className={cn("group/run flex min-h-11 w-full items-center gap-3 px-3.5 py-2 text-left outline-none transition-colors duration-150 hover:bg-fg/[0.03] focus-visible:bg-fg/[0.04]", r.id === selected && "bg-fg/[0.05]")}
            >
              <span className="w-[92px] shrink-0">
                <RunStatus status={r.status} />
              </span>
              <span className="min-w-0 flex-1">
                {showName && <span className="block truncate text-[13px] font-[510] tracking-[-0.01em]">{r.name}</span>}
                <span className={cn("block truncate text-[12px]", showName ? "text-subtle" : "text-soft")}>{r.reason && r.status !== "succeeded" ? r.reason : r.event.summary}</span>
              </span>
              <span className="hidden shrink-0 items-center gap-0.5 @md:flex" aria-label={`${done} of ${r.steps.length} steps done`}>
                {r.steps.map((s) => {
                  const Icon = stepIcon(s.kind);
                  return <Icon key={s.id} className={cn("size-3", s.status === "succeeded" ? "text-muted" : s.status === "failed" ? "text-danger" : s.status === "running" || s.status === "waiting" ? "text-accent" : "text-faint")} strokeWidth={1.75} />;
                })}
              </span>
              <span className="w-14 shrink-0 text-right font-mono text-[11px] text-subtle tabular-nums">{ms === null ? "" : duration(ms)}</span>
              <span className="w-16 shrink-0 text-right text-[11.5px] text-subtle tabular-nums" title={new Date(r.created_at).toLocaleString()}>
                {ago(r.created_at, now).replace(" ago", "")}
              </span>
              <ChevronRight className="size-3.5 shrink-0 text-faint transition-colors group-hover/run:text-muted" />
            </button>
          </li>
        );
      })}
    </ul>
  );
}

function AllRuns({ runs, automations, selected }: { runs: ReadonlyMap<string, AutomationRun>; automations: ReadonlyMap<string, Automation>; selected?: string }) {
  const { store } = useAutomations();
  const [filter, setFilter] = useState<"all" | RunState>("all");
  const [more, setMore] = useState<"idle" | "loading" | "end">("idle");
  const [now] = useState(() => Date.now());
  const all = useMemo(() => [...runs.values()].sort((a, b) => b.created_at - a.created_at), [runs]);
  const shown = all.filter((r) => filter === "all" || r.status === filter || (filter === "running" && (r.status === "waiting" || r.status === "queued")));
  return (
    <>
      <div className="mb-3 flex items-center gap-2">
        <Segmented label="Runs to show" value={filter} onChange={setFilter} options={FILTERS} />
        <span className="ml-auto text-[12px] text-subtle">{shown.length} shown</span>
      </div>
      {shown.length ? (
        <RunRows runs={shown} selected={selected} showName now={now} onOpen={(r) => setAgentsView({ kind: "automations", list: "runs", open: automations.has(r.automation) ? r.automation : undefined, tab: "runs", run: r.id })} />
      ) : (
        <p className="py-16 text-center text-[13px] text-subtle">{filter === "all" ? "No runs yet. Runs show up here the moment a trigger fires." : `No ${filter} runs.`}</p>
      )}
      {all.length >= 50 && more !== "end" && (
        <div className="mt-3 flex justify-center">
          <QuietButton
            disabled={more === "loading"}
            onClick={async () => {
              setMore("loading");
              const page = await store?.more(null, all.at(-1)?.id).catch(() => []);
              setMore(page && page.length ? "idle" : "end");
            }}
          >
            {more === "loading" ? <span className="shimmer">Loading…</span> : "Older runs"}
          </QuietButton>
        </div>
      )}
    </>
  );
}

// ── templates ──

export function useTemplateContext(): TemplateContext {
  const { client } = useAgents();
  const data = client.data;
  return useMemo(
    () => ({
      machines: [...data.machines.values()].map((r) => ({ id: r.value.id, name: r.value.name, online: r.value.online, sandbox: !!r.value.sandbox })),
      projects: [...data.projects.values()].map((r) => ({ id: r.value.id, name: r.value.name, machine: r.machine, repo: r.value.repo ? `${r.value.repo.owner}/${r.value.repo.repo}` : null })),
    }),
    [data.machines, data.projects],
  );
}

function Gallery({ open }: { open?: string }) {
  const { catalog } = useAutomations();
  return (
    <ul className="grid grid-cols-1 gap-3 @xl:grid-cols-2 @4xl:grid-cols-3" aria-label="Templates">
      {TEMPLATES.map((t) => {
        const info = t.integration ? catalog.triggers.find((x) => x.source === t.integration) : undefined;
        return (
          <li key={t.id}>
            <TemplateCard template={t} selected={open === t.id} needsSetup={!!info?.needs_setup} />
          </li>
        );
      })}
    </ul>
  );
}

const triggerWord: Record<string, string> = { manual: "By hand", schedule: "Schedule", interval: "Interval", webhook: "Webhook", code: "roll code", agent: "Agents" };

function TemplateCard({ template: t, selected, needsSetup }: { template: AutomationTemplate; selected: boolean; needsSetup: boolean }) {
  return (
    <button
      type="button"
      onClick={() => setAgentsView({ kind: "automations", open: "new", template: t.id })}
      aria-label={`Use the template ${t.title}`}
      className={cn(
        "group/tpl flex h-full w-full flex-col rounded-xl border bg-panel p-3.5 text-left shadow-card inset-shadow-highlight outline-none transition-[border-color,box-shadow,translate] duration-150 ease-out hover:-translate-y-px hover:border-line-3 hover:shadow-card-hover focus-visible:outline-2 focus-visible:outline-offset-2 focus-visible:outline-accent/70 motion-reduce:hover:translate-y-0",
        selected ? "border-accent/50" : "border-line-2",
      )}
    >
      <span className="flex items-center gap-1.5">
        <Tile icon={triggerIcon(t.integration ?? t.trigger)} className="text-soft" />
        <ArrowRight className="size-3 text-faint" aria-hidden />
        {t.steps.map((k, i) => {
          const Icon = stepIcon(k);
          return (
            <span key={i} className="grid size-6 place-items-center rounded-md border border-line bg-fg/[0.02] text-subtle" aria-hidden>
              <Icon className="size-3" strokeWidth={1.75} />
            </span>
          );
        })}
      </span>
      <span className="mt-3 block text-[13.5px] font-[590] tracking-[-0.015em]">{t.title}</span>
      <span className="mt-1 line-clamp-2 block text-[12.5px] leading-snug text-muted">{t.description}</span>
      <span className="mt-auto flex items-center gap-1.5 pt-3 text-[11px] text-subtle">
        <span className="rounded-[5px] border border-line-2 px-1.5 leading-[18px]">{t.integration ? (integrationOf(t.integration)?.name ?? t.integration[0].toUpperCase() + t.integration.slice(1)) : (triggerWord[t.trigger] ?? t.trigger)}</span>
        {needsSetup && <span className="rounded-[5px] border border-line-2 px-1.5 leading-[18px]">Connect first</span>}
        <span className="ml-auto flex items-center gap-1 text-muted opacity-0 transition-opacity duration-150 group-hover/tpl:opacity-100 group-focus-visible/tpl:opacity-100 [@media(hover:none)]:opacity-100">
          Use <ArrowRight className="size-3" />
        </span>
      </span>
    </button>
  );
}

function EmptyState() {
  return (
    <div className="relative isolate">
      <DotField className="pointer-events-none absolute inset-x-0 top-0 -z-10 h-72 text-fg [mask-image:linear-gradient(to_bottom,black,transparent)]" />
      <Section>
        <div className="mx-auto max-w-xl pt-12 pb-10 text-center max-sm:pt-6">
          <h2 className="text-[28px] leading-tight font-[590] tracking-[-0.05em]">Let your agents run on their own</h2>
          <p className="mx-auto mt-2 max-w-md text-[13.5px] leading-relaxed text-muted">
            When something happens (a schedule, a webhook, main breaking, an agent finishing), start agents on any machine or a fresh sandbox, wait for them, and ship what they made.
          </p>
          <div className="mt-5 flex justify-center gap-2">
            <PrimaryButton onClick={() => setAgentsView({ kind: "automations", open: "new" })}>
              <Plus strokeWidth={2} /> Start from scratch
            </PrimaryButton>
          </div>
        </div>
        <h3 className="mb-2.5 px-0.5 text-[12px] font-medium text-soft">Start from a template</h3>
        <Gallery />
      </Section>
    </div>
  );
}
