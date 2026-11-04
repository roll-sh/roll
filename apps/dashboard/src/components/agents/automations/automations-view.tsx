/**
 * The automations list, the root of the automations sheet (docs/code/automations-ui.md). Three
 * lists under one bar: the automations (two-line rows: trigger, name, when it runs, its last run,
 * seven days of runs, the switch), every run, and the templates; the empty state is the templates
 * gallery. Opening one goes a level into the sheet (sheet.tsx).
 */

import { DotField } from "@roll/brand";
import { ArrowRight, ChevronRight, Plus, Search, X, Zap } from "lucide-react";
import { useMemo, useState, type ReactNode } from "react";
import type { Automation, AutomationRun, RunState } from "@/lib/automations-api";
import { TEMPLATES, type AutomationTemplate, type TemplateContext } from "@/lib/automation-templates";
import { setAgentsView, type AutomationsView as View } from "@/lib/agents-surface";
import { duration, runLabel, runStart, took } from "@/lib/automations-ui";
import { automationPlaces } from "@/lib/agents-identity";
import { MachineIcon, ProjectIcon } from "../identity";
import { integrationOf } from "../brand-icons";
import { ago } from "../shared";
import { cn, Segmented } from "../ui";
import { useAgents } from "../use-agents";
import type { ListFilter as Filter } from "@/lib/automations-list";
import { AutomationList, FilterButton } from "./list";
import { NoticeList } from "./notices";
import { unseenNotices } from "@/lib/automations-store";
import { SheetHeader } from "./sheet";
import { PrimaryButton, QuietButton, RunDot, RunStatus, stepIcon, Tile, triggerIcon, useAutomations } from "./shared";

export function AutomationsView({ view, phone }: { view: View; phone: boolean }) {
  const a = useAutomations();
  const list = view.list ?? "automations";
  const go = (patch: Partial<View>) => setAgentsView({ kind: "automations", ...patch });
  const automations = useMemo(() => [...a.automations.values()].sort((x, y) => Number(y.enabled) - Number(x.enabled) || x.name.localeCompare(y.name)), [a.automations]);
  // Kept while the page is open, so coming back from an automation finds the list as it was.
  const [query, setQuery] = useState(() => kept.query);
  const [filter, setFilterState] = useState<Filter>(() => kept.filter);
  const setSearch = (q: string) => setQuery((kept.query = q));
  const setFilter = (f: Filter) => setFilterState((kept.filter = f));
  const empty = a.loaded && !a.error && automations.length === 0;
  const unseen = unseenNotices(a);

  if (!a.store)
    return (
      <>
        <SheetHeader title="Automations" />
        <Center title="Automations aren't on this server yet" body="Update roll to set up triggers that run your agents." />
      </>
    );
  return (
    <>
      <SheetHeader
        icon={<Tile icon={Zap} />}
        title="Automations"
        tools={
          <PrimaryButton onClick={() => go({ open: "new", list: view.list })} title="New automation">
            <Plus strokeWidth={2} />
            <span className="max-sm:sr-only">New</span>
          </PrimaryButton>
        }
      />
      <div className="flex min-h-11 shrink-0 flex-wrap items-center gap-2 px-3 py-2" aria-label="Automations">
        <Segmented
          label="Show"
          value={list}
          onChange={(v) => go({ list: v === "automations" ? undefined : v })}
          options={[
            { value: "automations", label: <>Automations{automations.length > 0 && <Count n={automations.length} />}</> },
            { value: "runs", label: <>Runs{running(a.runs) > 0 && <Count n={running(a.runs)} accent />}</> },
            { value: "notices", label: <>Notices{unseen > 0 && <Count n={unseen} accent />}</> },
            { value: "templates", label: "Templates" },
          ]}
        />
        {list === "automations" && automations.length > 0 && (
          <label className="ml-auto flex h-7 min-w-0 flex-1 basis-32 max-sm:order-last max-sm:ml-0 max-sm:basis-full items-center gap-2 rounded-md border border-line bg-fg/[0.03] px-2 text-[12.5px] inset-shadow-highlight transition-colors focus-within:border-line-3 sm:max-w-56">
            <Search className="size-3.5 shrink-0 text-subtle" aria-hidden />
            <input value={query} onChange={(e) => setSearch(e.target.value)} placeholder="Search" aria-label="Search automations" className="min-w-0 flex-1 bg-transparent outline-none placeholder:text-subtle" />
            {query && (
              <button type="button" onClick={() => setSearch("")} aria-label="Clear the search" className="text-subtle hover:text-fg">
                <X className="size-3" />
              </button>
            )}
          </label>
        )}
        {list === "automations" && automations.length > 1 && <FilterButton automations={automations} filter={filter} setFilter={setFilter} />}
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto @container" data-scroll>
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
        ) : list === "notices" ? (
          <Section>
            <NoticeList />
          </Section>
        ) : empty ? (
          <EmptyState />
        ) : (
          <Section>
            <AutomationList automations={automations} query={query} setQuery={setSearch} filter={filter} setFilter={setFilter} list={view.list} />
            <p className="mt-4 px-1 text-[12px] text-subtle">
              Automations run your agents when something happens.{" "}
              <button onClick={() => go({ list: "templates" })} className="text-accent hover:underline">
                Browse templates
              </button>
            </p>
          </Section>
        )}
      </div>
    </>
  );
}

const running = (runs: ReadonlyMap<string, AutomationRun>) => [...runs.values()].filter((r) => r.status === "running" || r.status === "waiting" || r.status === "queued").length;

function Count({ n, accent }: { n: number; accent?: boolean }) {
  return <span className={cn("ml-1 grid h-4 min-w-4 place-items-center rounded-full px-1 font-mono text-[10px] tabular-nums", accent ? "bg-accent/20 text-accent" : "bg-fg/[0.08] text-muted")}>{n}</span>;
}

/** The list's search and filters, kept while the page is open (going into an automation and back). */
const kept: { query: string; filter: Filter } = { query: "", filter: {} };

/** A fresh list the next time the sheet opens. */
export function forgetListState() {
  kept.query = "";
  kept.filter = {};
}

function Section({ children }: { children: ReactNode }) {
  return <div className="w-full px-3 pt-1 pb-8 max-sm:px-2">{children}</div>;
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

/** The projects and machines an automation works in, as the canvas shows them. */
export function usePlaces(automation: Automation) {
  const { client } = useAgents();
  const data = client.data;
  return useMemo(
    () =>
      automationPlaces(
        automation,
        [...data.projects.values()].map((r) => ({ ...r.value, machine: r.machine })),
        [...data.machines.values()].map((r) => r.value),
      ),
    [automation, data.projects, data.machines],
  );
}

/** Where it works, as a quiet stack of project and machine icons after its name. */
export function Places({ automation }: { automation: Automation }) {
  const { projects, machines } = usePlaces(automation);
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
        const ms = took(runStart(r), r.finished_at, now);
        const done = r.steps.filter((s) => s.status === "succeeded").length;
        return (
          <li key={r.id} className="border-line [&+&]:border-t">
            <button
              type="button"
              onClick={() => onOpen(r)}
              aria-current={r.id === selected || undefined}
              className={cn("group/run flex min-h-11 w-full items-center gap-3 px-3.5 py-2 text-left outline-none transition-colors duration-150 hover:bg-fg/[0.03] focus-visible:bg-fg/[0.04]", r.id === selected && "bg-fg/[0.05]")}
            >
              {/* Narrow (a phone): the status's dot and no duration, so the run's name has room. */}
              <span className="hidden w-[92px] shrink-0 @md:block">
                <RunStatus status={r.status} />
              </span>
              <span className="grid size-4 shrink-0 place-items-center @md:hidden" role="img" aria-label={runLabel[r.status]} title={runLabel[r.status]}>
                <RunDot status={r.status} />
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
              <span className="hidden w-14 shrink-0 text-right font-mono text-[11px] text-subtle tabular-nums @md:block">{ms === null ? "" : duration(ms)}</span>
              <span className="w-12 shrink-0 text-right text-[11.5px] text-subtle tabular-nums @md:w-16" title={new Date(r.created_at).toLocaleString()}>
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
      <DotField className="pointer-events-none absolute inset-x-0 top-0 -z-10 h-44 text-fg [mask-image:linear-gradient(to_bottom,black,transparent)]" />
      <Section>
        <div className="px-1 pt-6 pb-6">
          <h2 className="text-[20px] leading-tight font-[590] tracking-[-0.04em]">Let your agents run on their own</h2>
          <p className="mt-1.5 max-w-md text-[13px] leading-relaxed text-muted">
            When something happens (a schedule, a webhook, main breaking, an agent finishing), start agents on any machine or a fresh sandbox, wait for them, and ship what they made.
          </p>
          <div className="mt-4 flex gap-2">
            <PrimaryButton onClick={() => setAgentsView({ kind: "automations", open: "new" })}>
              <Plus strokeWidth={2} /> Start from scratch
            </PrimaryButton>
          </div>
        </div>
        <h3 className="mb-2.5 px-1 text-[12px] font-medium text-soft">Or start from a template</h3>
        <Gallery />
      </Section>
    </div>
  );
}
